/// ws/client.rs - 毫秒级 WebSocket 核心客户端
///
/// 设计要点：
/// 1. 接收消息后立即打时间戳（recv_ts_ns），最小化 jitter
/// 2. 通过 tokio::sync::broadcast channel 分发事件给多个消费者
/// 3. Ping/Pong 心跳保活（每 20s 发送 Ping）
/// 4. 自动重连（指数退避，见 reconnect.rs）
/// 5. 解析错误不中断连接，仅记录日志
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tokio::time::interval;
use tokio_tungstenite::{
    connect_async, tungstenite::protocol::Message, MaybeTlsStream, WebSocketStream,
};
use tracing::{debug, error, info, warn};

use crate::{
    config::AppConfig,
    metrics::latency::LatencyMonitor,
    tui::app::AppState,
    ws::{
        reconnect::ReconnectPolicy,
        stream::{CombinedStreamMsg, MarketEvent},
    },
};

/// 广播 channel 容量（缓冲消息数）
const CHANNEL_CAPACITY: usize = 1024;

/// WebSocket 客户端
pub struct BinanceWsClient {
    config: AppConfig,
    event_tx: broadcast::Sender<MarketEvent>,
    latency: LatencyMonitor,
    state: Arc<RwLock<AppState>>,
}

impl BinanceWsClient {
    /// 创建客户端
    pub fn new(
        config: AppConfig,
        state: Arc<RwLock<AppState>>,
    ) -> (Self, broadcast::Receiver<MarketEvent>) {
        let (event_tx, event_rx) = broadcast::channel(CHANNEL_CAPACITY);
        let latency = LatencyMonitor::new(
            config.latency.warn_threshold_ms,
            config.latency.report_interval_secs,
        );
        (
            Self {
                config,
                event_tx,
                latency,
                state,
            },
            event_rx,
        )
    }

    /// 订阅事件（获取一个新的 receiver）
    pub fn subscribe(&self) -> broadcast::Receiver<MarketEvent> {
        self.event_tx.subscribe()
    }

    /// 获取事件发送者的克隆（用于其他 WS 客户端共享通道）
    pub fn event_tx_clone(&self) -> broadcast::Sender<MarketEvent> {
        self.event_tx.clone()
    }

    /// 启动客户端主循环（永久运行，自动重连）
    pub async fn run(self) -> Result<()> {
        let ws_url = self.config.build_ws_url();
        info!("🚀 WebSocket 目标连接: {}", ws_url);

        let mut reconnect = ReconnectPolicy::new(
            self.config.websocket.reconnect_base_ms,
            self.config.websocket.reconnect_max_ms,
        );

        loop {
            match self.connect_and_recv(&mut reconnect, &ws_url).await {
                Ok(_) => {
                    warn!("WebSocket 连接正常关闭，准备重连...");
                }
                Err(e) => {
                    error!("WebSocket 错误: {:?}", e);
                }
            }
            // 断线后必须显式回落 ws_connected，否则 UI/策略会继续以为还连着
            if let Ok(mut s) = self.state.write() {
                s.ws_connected = false;
            }

            let delay = reconnect.next_delay();
            warn!(
                "第 {} 次重连，等待 {}ms...",
                reconnect.attempt(),
                delay.as_millis()
            );
            tokio::time::sleep(delay).await;
        }
    }

    /// 单次连接 + 消息接收循环
    async fn connect_and_recv(&self, reconnect: &mut ReconnectPolicy, ws_url: &str) -> Result<()> {
        info!("正在连接 Binance WebSocket...");
        let (ws_stream, response) = connect_async(ws_url).await.context("WebSocket 连接失败")?;

        info!("✅ WebSocket 已连接! HTTP Status: {}", response.status());
        // 连接成功立即重置退避计数：长跑后再次断线也从 base_ms 开始
        reconnect.reset();
        if let Ok(mut s) = self.state.write() {
            s.ws_connected = true;
        }

        // 显式类型注解，避免 Rust 类型推断失败
        let ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>> = ws_stream;
        let (mut ws_sink, mut ws_source) = ws_stream.split();

        // ── Ping 心跳定时器 ──────────────────────────────────
        let ping_interval_secs = self.config.websocket.ping_interval_secs;
        let pong_timeout_secs = self.config.websocket.pong_timeout_secs;
        let mut ping_ticker = interval(Duration::from_secs(ping_interval_secs));
        ping_ticker.tick().await; // 第一次不立即发送

        let mut last_pong = std::time::Instant::now();

        // ── 主消息循环 ───────────────────────────────────────
        loop {
            tokio::select! {
                // 1. 收到 WebSocket 消息
                msg_option = ws_source.next() => {
                    let msg_result = match msg_option {
                        Some(r) => r,
                        None => break,
                    };

                    // ⚡ 第一时间打时间戳！（在任何处理之前）
                    let recv_ts_ns = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos();

                    match msg_result {
                        Ok(Message::Text(text)) => {
                            let parse_start = std::time::Instant::now();

                            // 解析消息
                            match serde_json::from_str::<CombinedStreamMsg>(&text) {
                                Ok(msg) => {
                                    let parse_us = parse_start.elapsed().as_micros() as u64;

                                    // 记录延迟，并将快照写入共享状态
                                    let exchange_ts_ms = Self::extract_exchange_ts(&msg);
                                    if let Some(ts_ms) = exchange_ts_ms {
                                        let snap = self.latency.record_and_snapshot(ts_ms, recv_ts_ns, parse_us);
                                        if let Some(snap) = snap {
                                            if let Ok(mut s) = self.state.write() {
                                                s.latency = snap;
                                            }
                                        }
                                    }

                                    // 转换为 MarketEvent 并广播
                                    let event = msg.parse_event(recv_ts_ns);
                                    let _ = self.event_tx.send(event);
                                }
                                Err(e) => {
                                    warn!("JSON 解析失败: {} | raw: {:.100}", e, text);
                                }
                            }
                        }

                        Ok(Message::Pong(_)) => {
                            debug!("收到 Pong");
                            last_pong = std::time::Instant::now();
                        }

                        Ok(Message::Ping(data)) => {
                            // 服务器发来 Ping，立即回应 Pong
                            debug!("收到服务器 Ping，回应 Pong");
                            let _ = ws_sink.send(Message::Pong(data)).await;
                        }

                        Ok(Message::Close(frame)) => {
                            warn!("WebSocket 关闭帧: {:?}", frame);
                            break;
                        }

                        Err(e) => {
                            error!("WebSocket 读取错误: {}", e);
                            break;
                        }

                        _ => {} // Binary 等不处理
                    }
                }

                // 2. Ping 定时器触发
                _ = ping_ticker.tick() => {
                    // 检查 Pong 超时
                    if last_pong.elapsed() > Duration::from_secs(
                        ping_interval_secs + pong_timeout_secs
                    ) {
                        return Err(anyhow::anyhow!("Pong 超时，断线重连"));
                    }

                    debug!("发送 Ping...");
                    let _ = ws_sink.send(Message::Ping(vec![])).await;
                }
            }
        }
        Ok(())
    }

    /// 从消息数据中提取交易所时间戳（ms）
    fn extract_exchange_ts(msg: &CombinedStreamMsg) -> Option<u64> {
        msg.data.get("E").and_then(|v| v.as_u64())
    }
}
