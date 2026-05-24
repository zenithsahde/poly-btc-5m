//! Polymarket CLOB user-channel WSS：用 derived API 三件套（apiKey/secret/passphrase）
//! 在 `wss://.../ws/user` 上订阅本账户所有市场的 `order` / `trade` 事件，投递到 mpsc。
//!
//! 行为约定参考 docs.polymarket.com/api-reference/wss/user：
//!   * 首帧 `type=user`，省略 `markets` 字段 → 全市场。
//!   * 每 10s 发文本 "PING"，服务端回 "PONG"（与 market channel 同款）。
//!   * 不支持运行期改订阅集合；只在出错或 5min stale 时整连重连。

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior, interval, sleep};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, error, info, warn};

const WS_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/user";
const PING_INTERVAL: Duration = Duration::from_secs(10);
const STALE_TIMEOUT: Duration = Duration::from_secs(300);
const RECONNECT_BACKOFF: Duration = Duration::from_secs(3);

// === Wire messages ===

#[derive(Serialize)]
struct AuthPayload<'a> {
    #[serde(rename = "apiKey")]
    api_key: &'a str,
    secret: &'a str,
    passphrase: &'a str,
}

#[derive(Serialize)]
struct SubscribeCmd<'a> {
    auth: AuthPayload<'a>,
    #[serde(rename = "type")]
    sub_type: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    markets: Option<&'a [String]>,
}

// === Public types ===
//
// 容器级 `#[serde(default)]`：任何字段缺失自动走 `Default::default()`，
// 避免单字段缺失整条事件解析失败 —— 比每个字段挂 `#[serde(default)]` 干净一截。

/// `event_type = "order"`：服务端在 PLACEMENT / UPDATE / CANCELLATION 时推送。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UserOrderEvent {
    pub id: String,
    pub owner: String,
    pub market: String,
    pub asset_id: String,
    pub side: String,
    pub price: String,
    pub original_size: String,
    pub size_matched: String,
    pub status: String,
    /// PLACEMENT | UPDATE | CANCELLATION
    #[serde(rename = "type")]
    pub order_event_type: String,
    pub order_type: String,
    pub outcome: String,
    pub timestamp: String,
    pub expiration: Option<String>,
    pub created_at: Option<String>,
    pub maker_address: Option<String>,
    pub associate_trades: Option<Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct MakerOrderInfo {
    pub order_id: String,
    pub owner: String,
    pub matched_amount: String,
    pub price: String,
    pub asset_id: String,
    pub outcome: String,
    pub fee_rate_bps: Option<String>,
}

/// `event_type = "trade"`：MATCHED / MINED / CONFIRMED / RETRYING / FAILED。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct UserTradeEvent {
    pub id: String,
    pub taker_order_id: String,
    pub market: String,
    pub asset_id: String,
    pub side: String,
    pub size: String,
    pub price: String,
    pub status: String,
    pub owner: String,
    /// TAKER | MAKER
    pub trader_side: String,
    pub timestamp: String,
    pub fee_rate_bps: Option<String>,
    pub matchtime: Option<String>,
    pub last_update: Option<String>,
    pub transaction_hash: Option<String>,
    pub bucket_index: Option<i64>,
    pub maker_orders: Vec<MakerOrderInfo>,
}

#[derive(Debug, Clone)]
pub enum UserEvent {
    Order(UserOrderEvent),
    Trade(UserTradeEvent),
}

#[derive(Deserialize)]
#[serde(tag = "event_type", rename_all = "snake_case")]
enum WireEvent {
    Order(UserOrderEvent),
    Trade(UserTradeEvent),
    #[serde(other)]
    Other,
}

// === Client ===

pub struct PolyUserWs {
    api_key: String,
    secret: String,
    passphrase: String,
    out: mpsc::Sender<UserEvent>,
}

impl PolyUserWs {
    pub fn new(
        api_key: String,
        secret: String,
        passphrase: String,
        out: mpsc::Sender<UserEvent>,
    ) -> Self {
        Self {
            api_key,
            secret,
            passphrase,
            out,
        }
    }

    /// 永驻 task：连接出错或 5min 无消息 → backoff 后整连重连。
    pub async fn run(self: Arc<Self>) {
        loop {
            if let Err(e) = self.run_once().await {
                error!(error = format!("{e:#}"), "poly_user_ws session error; reconnecting");
            }
            sleep(RECONNECT_BACKOFF).await;
        }
    }

    async fn run_once(&self) -> Result<()> {
        let (ws, _) = connect_async(WS_URL).await?;
        let (mut writer, mut reader) = ws.split();

        let sub = serde_json::to_string(&SubscribeCmd {
            auth: AuthPayload {
                api_key: &self.api_key,
                secret: &self.secret,
                passphrase: &self.passphrase,
            },
            sub_type: "user",
            markets: None,
        })?;
        writer.send(Message::Text(sub)).await?;
        info!("poly_user_ws: connected & subscribed (all markets)");

        let mut ping = interval(PING_INTERVAL);
        ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut last_message = Instant::now();

        loop {
            tokio::select! {
                _ = ping.tick() => {
                    // Polymarket WSS 期望文本 "PING"（不是 Message::Ping），否则 5min 超时被踢。
                    if writer.send(Message::Text("PING".into())).await.is_err() {
                        bail!("ws send PING failed");
                    }
                }
                msg = reader.next() => {
                    last_message = Instant::now();
                    match msg {
                        Some(Ok(Message::Text(t))) => {
                            let text = t.to_string();
                            if !matches!(text.as_str(), "PONG" | "[]\n") {
                                self.dispatch_text(&text).await;
                            }
                        }
                        Some(Ok(Message::Ping(d))) => writer.send(Message::Pong(d)).await?,
                        Some(Ok(Message::Close(f))) => bail!("ws closed: {f:?}"),
                        Some(Ok(_)) => {}
                        Some(Err(e)) => bail!(e),
                        None => bail!("ws stream ended"),
                    }
                }
            }

            if last_message.elapsed() > STALE_TIMEOUT {
                bail!("ws stale {}s", STALE_TIMEOUT.as_secs());
            }
        }
    }

    async fn dispatch_text(&self, text: &str) {
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            return;
        };
        // Polymarket 一次可发数组也可发单对象。
        let items: Vec<Value> = match v {
            Value::Array(a) => a,
            other => vec![other],
        };
        for item in items {
            match serde_json::from_value::<WireEvent>(item) {
                Ok(WireEvent::Order(e)) => {
                    if self.out.send(UserEvent::Order(e)).await.is_err() {
                        warn!("poly_user_ws: out channel closed");
                    }
                }
                Ok(WireEvent::Trade(e)) => {
                    if self.out.send(UserEvent::Trade(e)).await.is_err() {
                        warn!("poly_user_ws: out channel closed");
                    }
                }
                Ok(WireEvent::Other) => {}
                Err(e) => debug!(error = %e, "poly_user_ws: event decode failed"),
            }
        }
    }
}
