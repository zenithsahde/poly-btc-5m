use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
/// ws/poly_client.rs - Polymarket CLOB WebSocket 客户端
/// 新端点: wss://ws-subscriptions-clob.polymarket.com/ws/market
/// 订阅: { "assets_ids": [token_id], "type": "market", "custom_feature_enabled": true }
/// 支持: 完整订单簿、自动在 5m 窗口结束时切换到下一市场、PING 保活
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use tracing::{error, info, warn};

use crate::tui::app::{AppState, BookLevel};
use crate::ws::discovery::{Active5mMarket, MarketDiscovery};
use crate::ws::reconnect::ReconnectPolicy;
use crate::ws::stream::{MarketEvent, PolyBookData};

const WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";
const PING_INTERVAL_SECS: u64 = 10;
const SWITCH_CHECK_INTERVAL_SECS: u64 = 10;
/// 重连退避：base 500ms，max 30s。switch_market_reconnect 不计入退避（视为正常切换）
const RECONNECT_BASE_MS: u64 = 500;
const RECONNECT_MAX_MS: u64 = 30_000;

#[derive(Debug, Serialize)]
struct PolySubscribeMsg {
    assets_ids: Vec<String>,
    #[serde(rename = "type")]
    typ: String,
    custom_feature_enabled: bool,
}

/// 可变的当前市场（Up + Down 双 token）+ 下一档预取
struct PolyMarketState {
    up_token_id: String,
    down_token_id: String,
    slug: String,
    window_end_ts: i64,
    next_market: Option<Active5mMarket>,
}

pub struct PolyWsClient {
    market: Arc<RwLock<PolyMarketState>>,
    event_tx: tokio::sync::broadcast::Sender<MarketEvent>,
    state: Arc<std::sync::RwLock<AppState>>,
    rest_endpoint: String,
    symbol: String,
}

impl PolyWsClient {
    pub fn new(
        initial: Active5mMarket,
        event_tx: tokio::sync::broadcast::Sender<MarketEvent>,
        state: Arc<std::sync::RwLock<AppState>>,
        rest_endpoint: String,
        symbol: String,
    ) -> Self {
        let market = Arc::new(RwLock::new(PolyMarketState {
            up_token_id: initial.up_token_id.clone(),
            down_token_id: initial.down_token_id.clone(),
            slug: initial.slug.clone(),
            window_end_ts: initial.window_end_ts,
            next_market: None,
        }));
        if let Ok(mut s) = state.write() {
            s.poly_market_slug = initial.slug.clone();
            s.poly_token_id = initial.up_token_id.clone();
            s.poly_down_token_id = initial.down_token_id.clone();
            s.poly_window_end_ts = initial.window_end_ts;
        }
        Self {
            market,
            event_tx,
            state,
            rest_endpoint,
            symbol,
        }
    }

    pub async fn run(self) -> Result<()> {
        let mut reconnect = ReconnectPolicy::new(RECONNECT_BASE_MS, RECONNECT_MAX_MS);
        loop {
            let was_market_switch;
            match self.connect_and_recv(&mut reconnect).await {
                Ok(_) => {
                    was_market_switch = false;
                    warn!("Poly WS 连接正常关闭，重连中...");
                }
                Err(e) => {
                    if e.to_string().contains("switch_market_reconnect") {
                        was_market_switch = true;
                        info!("Poly 切换市场后重连中...");
                    } else {
                        was_market_switch = false;
                        error!("Poly WS 错误: {:?}", e);
                    }
                }
            }
            if let Ok(mut s) = self.state.write() {
                s.poly_ws_connected = false;
            }
            // 切窗口的"重连"是正常流程，不计入退避；真正的故障才指数退避
            let delay = if was_market_switch {
                tokio::time::Duration::from_millis(RECONNECT_BASE_MS)
            } else {
                reconnect.next_delay()
            };
            if !was_market_switch {
                warn!(
                    "第 {} 次重连，等待 {}ms...",
                    reconnect.attempt(),
                    delay.as_millis()
                );
            }
            tokio::time::sleep(delay).await;
        }
    }

    async fn connect_and_recv(&self, reconnect: &mut ReconnectPolicy) -> Result<()> {
        let (mut ws_stream, _) = connect_async(WS_URL).await.context("Poly WS 连接失败")?;
        info!("✅ Polymarket CLOB 已连接 (Market Channel)");
        // 连接成功立即重置退避计数
        reconnect.reset();
        if let Ok(mut s) = self.state.write() {
            s.poly_ws_connected = true;
            // 重连成功 → 清空旧订单簿，避免基于断线期间的陈旧 best 价决策
            //   订阅 book 事件后服务端会推完整快照覆盖
            s.poly_bids.clear();
            s.poly_asks.clear();
            s.poly_down_bids.clear();
            s.poly_down_asks.clear();
        }

        let ids: Vec<String> = {
            let market = self.market.read().await;
            [market.up_token_id.clone(), market.down_token_id.clone()]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect()
        };
        if ids.is_empty() {
            return Ok(());
        }
        let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();

        // 初始订阅 Up + Down 双 token
        self.send_subscribe(&mut ws_stream, &id_refs).await?;
        self.prefetch_next_market().await;

        let mut last_ping = tokio::time::Instant::now();
        let mut last_switch_check = tokio::time::Instant::now();

        loop {
            let msg_result =
                tokio::time::timeout(std::time::Duration::from_secs(20), ws_stream.next()).await;

            match msg_result {
                Ok(Some(Ok(msg))) => {
                    if last_switch_check.elapsed().as_secs() >= SWITCH_CHECK_INTERVAL_SECS {
                        last_switch_check = tokio::time::Instant::now();
                        if let Err(e) = self.maybe_switch_market(&mut ws_stream).await {
                            if e.to_string().contains("switch_market_reconnect") {
                                return Err(e);
                            }
                            warn!("Poly 切换市场检查失败: {:?}", e);
                        }
                    }
                    let recv_ts_ns = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos();
                    match msg {
                        Message::Text(text) => {
                            if text == "PONG" {
                                continue;
                            }
                            if let Err(e) = self
                                .handle_ws_message(&text, recv_ts_ns, &mut ws_stream)
                                .await
                            {
                                warn!("Poly 消息处理: {:?}", e);
                            }
                        }
                        Message::Ping(p) => {
                            let _ = ws_stream.send(Message::Pong(p)).await;
                        }
                        _ => {}
                    }
                }
                Ok(Some(Err(e))) => return Err(e.into()),
                Ok(None) => break,
                Err(_) => {
                    if last_ping.elapsed().as_secs() >= PING_INTERVAL_SECS {
                        let _ = ws_stream.send(Message::Text("PING".to_string())).await;
                        last_ping = tokio::time::Instant::now();
                    }
                    if last_switch_check.elapsed().as_secs() >= SWITCH_CHECK_INTERVAL_SECS {
                        last_switch_check = tokio::time::Instant::now();
                        if let Err(e) = self.maybe_switch_market(&mut ws_stream).await {
                            if e.to_string().contains("switch_market_reconnect") {
                                return Err(e);
                            }
                            warn!("Poly 切换市场检查失败: {:?}", e);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    async fn send_subscribe(
        &self,
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        token_ids: &[&str],
    ) -> Result<()> {
        let msg = PolySubscribeMsg {
            assets_ids: token_ids.iter().map(|s| (*s).to_string()).collect(),
            typ: "market".to_string(),
            custom_feature_enabled: true,
        };
        let json = serde_json::to_string(&msg)?;
        ws.send(Message::Text(json)).await?;
        Ok(())
    }

    /// 预取下一档市场（不阻塞主循环，到点直接取 next 用）
    async fn prefetch_next_market(&self) {
        let window_end = self.market.read().await.window_end_ts;
        match MarketDiscovery::get_market_for_window(window_end).await {
            Ok(next) => {
                let mut m = self.market.write().await;
                m.next_market = Some(next);
                info!("📥 已预取下一档市场，到点直接切换");
            }
            Err(e) => {
                warn!("预取下一档失败: {:?}", e);
            }
        }
    }

    /// 若当前时间 >= window_end_ts，更新状态并触发重连，新连接只订阅新市场以拿到完整 book
    async fn maybe_switch_market(
        &self,
        _ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        let new_market_opt = {
            let mut m = self.market.write().await;
            if now < m.window_end_ts {
                if m.next_market.is_none() {
                    drop(m);
                    self.prefetch_next_market().await;
                }
                return Ok(());
            }
            m.next_market.take()
        };

        let new_market = match new_market_opt {
            Some(n) => {
                info!("🔄 5m 窗口结束，使用预取市场直接切换（无 API 延迟）");
                n
            }
            None => {
                info!("🔄 5m 窗口结束，预取未就绪，现场拉取...");
                MarketDiscovery::get_active_5m_btc_market().await?
            }
        };

        // 只更新内存状态并清空订单簿，不在此连接上 unsubscribe/subscribe（服务端同连接换订往往不重推 book）
        {
            let mut m = self.market.write().await;
            m.up_token_id = new_market.up_token_id.clone();
            m.down_token_id = new_market.down_token_id.clone();
            m.slug = new_market.slug.clone();
            m.window_end_ts = new_market.window_end_ts;
        }
        // 行权价 K：优先用币安在「窗口开始这一秒」内的首笔成交价（秒级），无成交则 1m 开盘价，失败则用当前 mid
        let window_start_ts = new_market.window_end_ts - 300;
        let strike_from_binance = crate::ws::binance_rest::get_spot_price_at_time(
            &self.rest_endpoint,
            &self.symbol,
            window_start_ts,
        )
        .await;
        if let Ok(mut s) = self.state.write() {
            let old_window_ts = s.poly_window_end_ts;
            if old_window_ts != 0 {
                if let Err(e) = s.save_trades_for_window_and_clear(old_window_ts) {
                    tracing::warn!("保存窗口 {} 成交记录失败: {:?}", old_window_ts, e);
                }
            }
            // v0.4.3-5m: 窗口切换前先 settle —— force-merge 可配对 + redeem 单边赢家
            //   binance_close 用 last mid 作 BTC 收盘价代理（与 Chainlink 有 ~10$ basis 误差）
            //   这样残仓的真实结算结果会进入 cash_received，不再被 reset 吞掉
            let now_ms = chrono::Utc::now().timestamp_millis();
            let binance_close = s.mid_price;
            s.settle_window_and_redeem(binance_close, now_ms);
            s.reset_inventory_for_new_window();
            if old_window_ts != 0 {
                if let Err(e) = s.save_orders_for_window_and_clear(old_window_ts) {
                    tracing::warn!("保存窗口 {} 订单记录失败: {:?}", old_window_ts, e);
                }
            }
            s.poly_market_slug = new_market.slug.clone();
            s.poly_token_id = new_market.up_token_id.clone();
            s.poly_down_token_id = new_market.down_token_id.clone();
            s.poly_window_end_ts = new_market.window_end_ts;
            s.strike_price = match strike_from_binance {
                Ok(open) => {
                    info!(
                        "K 取自币安窗口开始秒内首笔成交: {:.2} (ts={})",
                        open, window_start_ts
                    );
                    open.round()
                }
                Err(e) => {
                    tracing::warn!("币安 K 线取 K 失败，回退 mid: {:?}", e);
                    s.mid_price.round()
                }
            };
            s.poly_bids.clear();
            s.poly_asks.clear();
            s.poly_best_bid = 0.0;
            s.poly_best_ask = 0.0;
            s.poly_last_trade_price = 0.0;
            s.poly_down_bids.clear();
            s.poly_down_asks.clear();
            s.poly_down_best_bid = 0.0;
            s.poly_down_best_ask = 0.0;
            s.poly_down_last_trade_price = 0.0;
        }
        info!(
            "✅ 已切换到: {} 窗口结束: {}（将重连以获取新订单簿）",
            new_market.slug, new_market.window_end_ts
        );

        self.prefetch_next_market().await;
        // 主动重连：新连接只订阅当前市场，服务端会推送完整 book 快照
        Err(anyhow::anyhow!("switch_market_reconnect"))
    }

    async fn handle_ws_message(
        &self,
        text: &str,
        recv_ts_ns: u128,
        _ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
    ) -> Result<()> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(());
        }
        let msgs: Vec<serde_json::Value> = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => {
                match serde_json::from_str::<serde_json::Value>(text) {
                    Ok(single) => vec![single],
                    Err(_) => {
                        // 服务端可能推送非 JSON（如订阅确认、空行等），忽略即可
                        tracing::debug!(
                            "Poly 忽略非 JSON 消息: {:?}",
                            if text.len() > 80 {
                                format!("{}...", &text[..80])
                            } else {
                                text.to_string()
                            }
                        );
                        return Ok(());
                    }
                }
            }
        };

        let (up_id, down_id) = {
            let m = self.market.read().await;
            (m.up_token_id.clone(), m.down_token_id.clone())
        };

        for m in msgs {
            let et = m
                .get("event_type")
                .or_else(|| m.get("type"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            // asset_id 可能是字符串或数字，统一成字符串再比较
            let asset_id = m
                .get("asset_id")
                .map(|v| {
                    v.as_str()
                        .map(String::from)
                        .or_else(|| v.as_i64().map(|n| n.to_string()))
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            let is_up = asset_id == up_id;

            match et {
                "book" => {
                    if let Ok(data) = serde_json::from_value::<PolyBookData>(m.clone()) {
                        // 解析后也兼容数字型 asset_id
                        let aid = if !data.asset_id.is_empty() {
                            data.asset_id.clone()
                        } else {
                            asset_id.clone()
                        };
                        let up = aid == up_id;
                        let down = aid == down_id;
                        if !up && !down {
                            warn!(
                                "Poly book 收到未知 asset_id，未写入订单簿 | asset_id={} 当前 up={} down={}",
                                aid, up_id, down_id
                            );
                        } else {
                            tracing::debug!(
                                "Poly book 已应用 | asset_id={} is_up={} bids={} asks={}",
                                aid,
                                up,
                                data.bids.len(),
                                data.asks.len()
                            );
                        }
                        let _ = self.event_tx.send(MarketEvent::PolyBookUpdate {
                            data: data.clone(),
                            recv_ts_ns,
                        });
                        self.apply_full_book_to_state(&data, up);
                    } else {
                        warn!(
                            "Poly book 解析失败 | raw event_type={} asset_id={}",
                            et, asset_id
                        );
                    }
                }
                "best_bid_ask" => {
                    let best_bid = m
                        .get("best_bid")
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse::<f64>().ok())
                        .unwrap_or(0.0);
                    let best_ask = m
                        .get("best_ask")
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse::<f64>().ok())
                        .unwrap_or(0.0);
                    if let Ok(mut s) = self.state.write() {
                        if is_up {
                            s.poly_best_bid = best_bid;
                            s.poly_best_ask = best_ask;
                        } else {
                            s.poly_down_best_bid = best_bid;
                            s.poly_down_best_ask = best_ask;
                        }
                        s.poly_last_update = Some(Instant::now());
                    }
                }
                "price_change" => {
                    if let Some(arr) = m.get("price_changes").and_then(|v| v.as_array()) {
                        for pc in arr {
                            let pid = pc.get("asset_id").and_then(|v| v.as_str()).unwrap_or("");
                            let up = pid == up_id;
                            let best_bid = pc
                                .get("best_bid")
                                .and_then(|v| v.as_str())
                                .and_then(|s| s.parse::<f64>().ok())
                                .unwrap_or(0.0);
                            let best_ask = pc
                                .get("best_ask")
                                .and_then(|v| v.as_str())
                                .and_then(|s| s.parse::<f64>().ok())
                                .unwrap_or(0.0);
                            if (best_bid > 0.0 || best_ask > 0.0) && (up || pid == down_id) {
                                if let Ok(mut s) = self.state.write() {
                                    if up {
                                        s.poly_best_bid = best_bid;
                                        s.poly_best_ask = best_ask;
                                    } else {
                                        s.poly_down_best_bid = best_bid;
                                        s.poly_down_best_ask = best_ask;
                                    }
                                    s.poly_last_update = Some(Instant::now());
                                }
                            }
                        }
                    }
                }
                "last_trade_price" => {
                    let price = m
                        .get("price")
                        .and_then(|v| v.as_str())
                        .and_then(|s| s.parse::<f64>().ok())
                        .unwrap_or(0.0);
                    let side = m
                        .get("side")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if let Ok(mut s) = self.state.write() {
                        if is_up {
                            s.poly_last_trade_price = price;
                            s.poly_last_trade_side = side.clone();
                        } else {
                            s.poly_down_last_trade_price = price;
                            s.poly_down_last_trade_side = side;
                        }
                        s.poly_last_update = Some(Instant::now());
                    }
                }
                other => {
                    if !other.is_empty() {
                        tracing::debug!("Poly 未处理 event_type={} asset_id={}", other, asset_id);
                    }
                }
            }
        }
        Ok(())
    }

    /// 解析 Poly book 事件：不依赖 API 返回顺序，统一按展示惯例排序。
    /// 展示：Asks 区从上到下价格降序（best ask 在最下、紧贴分隔线），Bids 区从上到下价格降序（best bid 在最上）。
    fn apply_full_book_to_state(&self, data: &PolyBookData, is_up: bool) {
        // Bids：解析后按价格降序，best bid 在首
        let mut bids: Vec<BookLevel> = data
            .bids
            .iter()
            .filter_map(|l| {
                let price = l.price.parse::<f64>().ok()?;
                let qty = l.size.parse::<f64>().ok()?;
                Some(BookLevel { price, qty })
            })
            .collect();
        bids.sort_by(|a, b| {
            b.price
                .partial_cmp(&a.price)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        bids.truncate(15);
        let best_bid = bids.first().map(|l| l.price).unwrap_or(0.0);

        // Asks：取最接近盘口的 15 档（最低的 15 个卖价），升序存 vec[0]=best ask；展示时倒序则 best ask 在最后一行
        let mut asks: Vec<BookLevel> = data
            .asks
            .iter()
            .filter_map(|l| {
                let price = l.price.parse::<f64>().ok()?;
                let qty = l.size.parse::<f64>().ok()?;
                Some(BookLevel { price, qty })
            })
            .collect();
        asks.sort_by(|a, b| {
            a.price
                .partial_cmp(&b.price)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        asks.truncate(15);
        let best_ask = asks.first().map(|l| l.price).unwrap_or(0.0);
        let last_trade = data
            .last_trade_price
            .as_ref()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        if let Ok(mut s) = self.state.write() {
            if is_up {
                s.poly_bids = bids;
                s.poly_asks = asks;
                s.poly_best_bid = best_bid;
                s.poly_best_ask = best_ask;
                if last_trade > 0.0 {
                    s.poly_last_trade_price = last_trade;
                }
            } else {
                s.poly_down_bids = bids;
                s.poly_down_asks = asks;
                s.poly_down_best_bid = best_bid;
                s.poly_down_best_ask = best_ask;
                if last_trade > 0.0 {
                    s.poly_down_last_trade_price = last_trade;
                }
            }
            s.poly_last_update = Some(Instant::now());
        }
    }
}
