/// ws/chainlink_ds.rs - Chainlink Data Streams 客户端（WS + REST）
///
/// 端点：
///   WS:   wss://ws.dataengine.chain.link/api/v1/ws?feedIDs=<feed_id>
///   REST: https://api.dataengine.chain.link/api/v1/reports?feedID=<id>&timestamp=<unix>
///
/// 认证（HMAC-SHA256）：
///   string-to-sign = "METHOD FULL_PATH BODY_HASH API_KEY TIMESTAMP_MS" (空格连接)
///   3 个 headers:
///     Authorization                       = <API_KEY>            (UUID)
///     X-Authorization-Timestamp           = <unix_ms_str>
///     X-Authorization-Signature-SHA256    = hex(HMAC-SHA256(secret, str_to_sign))
///
/// 报文：服务端推 JSON { "report": { "fullReport": "0x<hex>" } }，
///   `fullReport` 是 ABI 编码的 `bytes32[3] reportContext + bytes reportBlob`；
///   去掉 wrapper 后得到 reportBlob = 9×32-byte words = ReportDataV3：
///     [0]  feedId            bytes32
///     [1]  validFrom         uint32 (右对齐)
///     [2]  observationsTs    uint32 (右对齐)
///     [3]  nativeFee         uint192
///     [4]  linkFee           uint192
///     [5]  expiresAt         uint32 (右对齐)
///     [6]  benchmarkPrice    int192 (1e18 scaling)
///     [7]  bid               int192 (1e18 scaling)
///     [8]  ask               int192 (1e18 scaling)
///
/// 设计：单一 feed（BTC/USD），全局只跑一个 WS task；
/// REST 仅在窗口翻滚时拿 K，频率低。WS 断开走指数退避重连。

use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

use crate::model::chainlink::ChainlinkPriceData;
use crate::tui::app::AppState;
use crate::ws::reconnect::ReconnectPolicy;
use crate::ws::stream::MarketEvent;

type HmacSha256 = Hmac<Sha256>;

const PRICE_SCALE: f64 = 1e18;
const WS_PING_INTERVAL_MS: u64 = 20_000;
const RECONNECT_BASE_MS: u64 = 500;
const RECONNECT_MAX_MS: u64 = 30_000;
const WS_READ_BUFFER_MAX: usize = 4 * 1024 * 1024;

/// Chainlink Data Streams 客户端配置。
#[derive(Clone, Debug)]
pub struct ChainlinkConfig {
    pub api_key: String,
    pub api_secret: String,
    pub ws_endpoint: String,   // e.g. "wss://ws.dataengine.chain.link"
    pub rest_endpoint: String, // e.g. "https://api.dataengine.chain.link"
    pub feed_id: String,       // 0x00039d9e... (BTC/USD account-scoped)
}

impl ChainlinkConfig {
    /// 从环境变量读取（与 Python validator 一致）：
    ///   CHAINLINK_DS_API_KEY            必需
    ///   CHAINLINK_DS_API_SECRET         必需
    ///   CHAINLINK_DS_WS_BASE            可选，默认 wss://ws.dataengine.chain.link
    ///   CHAINLINK_DS_REST_BASE          可选，默认 https://api.dataengine.chain.link
    ///   CHAINLINK_DS_FEED_ID_BTCUSD     可选，默认 0x00039d9e45394f473ab1f050a1b963e6b05351e52d71e507509ada0c95ed75b8
    pub fn from_env() -> Result<Self> {
        let api_key =
            std::env::var("CHAINLINK_DS_API_KEY").context("CHAINLINK_DS_API_KEY 未设置")?;
        let api_secret =
            std::env::var("CHAINLINK_DS_API_SECRET").context("CHAINLINK_DS_API_SECRET 未设置")?;
        let ws_endpoint = std::env::var("CHAINLINK_DS_WS_BASE")
            .unwrap_or_else(|_| "wss://ws.dataengine.chain.link".to_string());
        let rest_endpoint = std::env::var("CHAINLINK_DS_REST_BASE")
            .unwrap_or_else(|_| "https://api.dataengine.chain.link".to_string());
        let feed_id = std::env::var("CHAINLINK_DS_FEED_ID_BTCUSD").unwrap_or_else(|_| {
            "0x00039d9e45394f473ab1f050a1b963e6b05351e52d71e507509ada0c95ed75b8".to_string()
        });
        Ok(Self {
            api_key,
            api_secret,
            ws_endpoint,
            rest_endpoint,
            feed_id,
        })
    }
}

/// HMAC-SHA256 签名 3 元组。
/// string-to-sign 与 Python 端严格一致："METHOD FULL_PATH BODY_HASH API_KEY TS_MS"
fn sign_request(
    method: &str,
    full_path: &str,
    body: &[u8],
    api_key: &str,
    api_secret: &str,
) -> Result<(String, String, String)> {
    let ts_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("time before epoch")?
        .as_millis()
        .to_string();
    let body_hash = format!("{:x}", Sha256::digest(body));
    let str_to_sign = format!(
        "{} {} {} {} {}",
        method.to_uppercase(),
        full_path,
        body_hash,
        api_key,
        ts_ms
    );
    let mut mac = HmacSha256::new_from_slice(api_secret.as_bytes())
        .map_err(|e| anyhow!("HMAC key invalid: {e}"))?;
    mac.update(str_to_sign.as_bytes());
    let sig = mac.finalize().into_bytes();
    let sig_hex = hex::encode(sig);
    Ok((api_key.to_string(), ts_ms, sig_hex))
}

/// 剥掉 fullReport 外层 (bytes32[3] reportContext + bytes reportBlob) 拿到 reportBlob。
/// 布局：payload[0..96] = reportContext，payload[96..128] = offset(uint256, big-endian)
/// payload[offset..offset+32] = length(uint256)，之后 length 字节是 reportBlob。
fn decode_full_report(payload: &[u8]) -> Result<Vec<u8>> {
    if payload.len() < 128 {
        return Err(anyhow!("payload too short: {} bytes", payload.len()));
    }
    // offset 在 payload[96..128] 的最低 8 字节（u64 足够）
    let offset = u64::from_be_bytes(
        payload[96 + 24..128]
            .try_into()
            .map_err(|_| anyhow!("offset slice"))?,
    ) as usize;
    if offset < 128 || offset + 32 > payload.len() {
        return Err(anyhow!("invalid offset {}", offset));
    }
    let length = u64::from_be_bytes(
        payload[offset + 24..offset + 32]
            .try_into()
            .map_err(|_| anyhow!("length slice"))?,
    ) as usize;
    let start = offset + 32;
    if start + length > payload.len() {
        return Err(anyhow!("invalid length {}", length));
    }
    Ok(payload[start..start + length].to_vec())
}

/// 把 32-byte big-endian 的有符号 int 解码成 i128（够装 int192 的低 16 字节范围）。
/// 在 chainlink ReportDataV3 中 benchmark/bid/ask 是 int192，1e18 scaling；
/// BTC 价大约 $1e5 量级 → 1e5 × 1e18 = 1e23，约 78 bits，i128 完全够装。
fn decode_int192_be(bytes: &[u8]) -> Result<i128> {
    if bytes.len() != 32 {
        return Err(anyhow!("int192 slice must be 32 bytes"));
    }
    // 取尾部 16 字节作为 i128。高 16 字节应该是符号扩展（0xff... 或 0x00...）。
    // 我们只信任 i128 表征范围内的数；超出则视为溢出。
    let high = &bytes[..16];
    let low = &bytes[16..];
    let sign_bit = bytes[0] & 0x80 != 0;
    if sign_bit {
        // 负数：要求高 16 字节全为 0xff
        if high.iter().any(|&b| b != 0xff) {
            return Err(anyhow!("int192 overflow (negative)"));
        }
    } else {
        // 正数：要求高 16 字节全为 0x00
        if high.iter().any(|&b| b != 0x00) {
            return Err(anyhow!("int192 overflow (positive)"));
        }
    }
    Ok(i128::from_be_bytes(
        low.try_into().map_err(|_| anyhow!("low slice"))?,
    ))
}

/// 解码 9-word ReportDataV3 blob → ChainlinkPriceData。
pub fn decode_report_v3(blob: &[u8]) -> Result<ChainlinkPriceData> {
    const WORD: usize = 32;
    if blob.len() < 9 * WORD {
        return Err(anyhow!("blob too short: {} bytes", blob.len()));
    }
    let feed_id = format!("0x{}", hex::encode(&blob[0..WORD]));
    // valid_from / observations_ts / expires_at 是 uint32，存在 word 的最后 4 字节
    let valid_from = u32::from_be_bytes(
        blob[1 * WORD + 28..2 * WORD]
            .try_into()
            .map_err(|_| anyhow!("valid_from slice"))?,
    ) as i64;
    let observation_ts = u32::from_be_bytes(
        blob[2 * WORD + 28..3 * WORD]
            .try_into()
            .map_err(|_| anyhow!("obs_ts slice"))?,
    ) as i64;
    let expires_at = u32::from_be_bytes(
        blob[5 * WORD + 28..6 * WORD]
            .try_into()
            .map_err(|_| anyhow!("expires_at slice"))?,
    ) as i64;
    let benchmark_raw = decode_int192_be(&blob[6 * WORD..7 * WORD])?;
    let bid_raw = decode_int192_be(&blob[7 * WORD..8 * WORD])?;
    let ask_raw = decode_int192_be(&blob[8 * WORD..9 * WORD])?;
    Ok(ChainlinkPriceData {
        feed_id,
        price: benchmark_raw as f64 / PRICE_SCALE,
        bid: bid_raw as f64 / PRICE_SCALE,
        ask: ask_raw as f64 / PRICE_SCALE,
        observation_ts,
        valid_from,
        expires_at,
    })
}

/// REST 端：取指定 unix 秒（或最近一个）的 report，用于窗口开盘价 K。
pub async fn fetch_report_at(
    config: &ChainlinkConfig,
    target_unix_ts: i64,
) -> Result<ChainlinkPriceData> {
    let full_path = format!(
        "/api/v1/reports?feedID={}&timestamp={}",
        config.feed_id, target_unix_ts
    );
    let url = format!("{}{}", config.rest_endpoint, full_path);
    let (auth, ts_ms, sig) = sign_request(
        "GET",
        &full_path,
        b"",
        &config.api_key,
        &config.api_secret,
    )?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let resp = client
        .get(&url)
        .header("Authorization", auth)
        .header("X-Authorization-Timestamp", ts_ms)
        .header("X-Authorization-Signature-SHA256", sig)
        .send()
        .await
        .context("chainlink REST request failed")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "chainlink REST HTTP {}: {}",
            status,
            &body[..body.len().min(200)]
        ));
    }
    let v: serde_json::Value = resp.json().await.context("chainlink REST JSON parse")?;
    let full_report = v
        .pointer("/report/fullReport")
        .and_then(|x| x.as_str())
        .ok_or_else(|| anyhow!("missing /report/fullReport in response"))?;
    let hex_str = full_report.strip_prefix("0x").unwrap_or(full_report);
    let payload = hex::decode(hex_str).context("hex decode")?;
    let blob = decode_full_report(&payload)?;
    decode_report_v3(&blob)
}

/// Chainlink Data Streams WS 客户端。
/// 单一 feed、单一 task；事件经 `event_tx` 广播给 SignalEngine / TUI。
pub struct ChainlinkDsClient {
    config: ChainlinkConfig,
    event_tx: broadcast::Sender<MarketEvent>,
    state: Arc<RwLock<AppState>>,
}

impl ChainlinkDsClient {
    pub fn new(
        config: ChainlinkConfig,
        event_tx: broadcast::Sender<MarketEvent>,
        state: Arc<RwLock<AppState>>,
    ) -> Self {
        Self {
            config,
            event_tx,
            state,
        }
    }

    /// 主循环：连接 → 接收报文 → 解码 → 广播；断开则按 ReconnectPolicy 退避重连。
    pub async fn run(self) -> Result<()> {
        let mut reconnect = ReconnectPolicy::new(RECONNECT_BASE_MS, RECONNECT_MAX_MS);
        loop {
            match self.connect_and_recv().await {
                Ok(_) => {
                    warn!("[chainlink-ws] 连接正常关闭，准备重连");
                }
                Err(e) => {
                    error!("[chainlink-ws] 错误: {} — 准备重连", e);
                }
            }
            let delay = reconnect.next_delay();
            tokio::time::sleep(delay).await;
        }
    }

    async fn connect_and_recv(&self) -> Result<()> {
        let full_path = format!("/api/v1/ws?feedIDs={}", self.config.feed_id);
        let url = format!("{}{}", self.config.ws_endpoint, full_path);
        let (auth, ts_ms, sig) = sign_request(
            "GET",
            &full_path,
            b"",
            &self.config.api_key,
            &self.config.api_secret,
        )?;
        let mut request = url.as_str().into_client_request().context("WS URL")?;
        let headers = request.headers_mut();
        headers.insert(
            "Authorization",
            HeaderValue::from_str(&auth).context("auth header")?,
        );
        headers.insert(
            "X-Authorization-Timestamp",
            HeaderValue::from_str(&ts_ms).context("ts header")?,
        );
        headers.insert(
            "X-Authorization-Signature-SHA256",
            HeaderValue::from_str(&sig).context("sig header")?,
        );
        let mut config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
        config.max_message_size = Some(WS_READ_BUFFER_MAX);
        config.max_frame_size = Some(WS_READ_BUFFER_MAX);

        info!("[chainlink-ws] 连接 {} feed={}", self.config.ws_endpoint, &self.config.feed_id[..18]);
        let (ws_stream, _) =
            tokio_tungstenite::connect_async_with_config(request, Some(config), false)
                .await
                .context("chainlink WS connect failed")?;
        info!("[chainlink-ws] 已连接");

        let (_write, mut read) = ws_stream.split();
        let mut last_ping = std::time::Instant::now();
        loop {
            tokio::select! {
                msg = read.next() => {
                    match msg {
                        Some(Ok(Message::Text(text))) => self.handle_text(&text).await,
                        Some(Ok(Message::Binary(bytes))) => {
                            // 二进制消息：尝试当作 utf-8 文本解析
                            match std::str::from_utf8(&bytes) {
                                Ok(text) => self.handle_text(text).await,
                                Err(_) => debug!("[chainlink-ws] 收到二进制 {} 字节，跳过", bytes.len()),
                            }
                        }
                        Some(Ok(Message::Ping(_))) => debug!("[chainlink-ws] ping 收到"),
                        Some(Ok(Message::Pong(_))) => debug!("[chainlink-ws] pong 收到"),
                        Some(Ok(Message::Close(_))) => {
                            warn!("[chainlink-ws] 远端关闭");
                            return Ok(());
                        }
                        Some(Ok(_)) => {}
                        Some(Err(e)) => return Err(anyhow!("ws recv err: {e}")),
                        None => return Err(anyhow!("ws stream ended")),
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(WS_PING_INTERVAL_MS)) => {
                    // 简化：依赖 tungstenite 自动 ping/pong；这里只更新一下时间戳监控
                    if last_ping.elapsed() > Duration::from_secs(60) {
                        warn!("[chainlink-ws] 60s 无心跳，主动断开重连");
                        return Ok(());
                    }
                    last_ping = std::time::Instant::now();
                }
            }
        }
    }

    async fn handle_text(&self, text: &str) {
        // 服务端格式：{"report": {"fullReport": "0x..."}}
        let v: serde_json::Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(e) => {
                warn!("[chainlink-ws] JSON 解析失败: {} | raw len={}", e, text.len());
                return;
            }
        };
        let full_report = match v.pointer("/report/fullReport").and_then(|x| x.as_str()) {
            Some(s) => s,
            None => {
                debug!("[chainlink-ws] 无 fullReport 字段，忽略");
                return;
            }
        };
        let hex_str = full_report.strip_prefix("0x").unwrap_or(full_report);
        let payload = match hex::decode(hex_str) {
            Ok(p) => p,
            Err(e) => {
                warn!("[chainlink-ws] hex 解码失败: {}", e);
                return;
            }
        };
        let blob = match decode_full_report(&payload) {
            Ok(b) => b,
            Err(e) => {
                warn!("[chainlink-ws] reportBlob 解码失败: {}", e);
                return;
            }
        };
        let report = match decode_report_v3(&blob) {
            Ok(r) => r,
            Err(e) => {
                warn!("[chainlink-ws] ReportDataV3 解码失败: {}", e);
                return;
            }
        };
        let recv_ts_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);

        // 写 AppState（供 TUI / FV 引擎读）
        if let Ok(mut s) = self.state.write() {
            s.chainlink_price = Some(report.price);
            s.chainlink_obs_ts = Some(report.observation_ts);
            s.chainlink_recv_ts_ms = Some((recv_ts_ns / 1_000_000) as i64);
        }

        // 广播事件（供 SignalEngine 用于 σ 计算 / jump 检测 / 历史维护）
        let _ = self.event_tx.send(MarketEvent::ChainlinkPrice {
            data: report,
            recv_ts_ns,
        });
    }
}
