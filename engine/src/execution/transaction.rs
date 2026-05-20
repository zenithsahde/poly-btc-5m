use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE;
use base64::Engine as Base64Engine;
use bytes::Bytes;
use hmac::{Hmac, Mac};
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Client, Method};
use serde_json::json;
use sha2::Sha256;
use tracing::{debug, info, warn};

use crate::execution::circuit_breaker::CircuitBreaker;
use crate::execution::client::{
    OrderClient, OrderSide, OrderType, PlaceOrderRequest, PlaceOrderResult,
};
use crate::execution::merge::{MergeClient, MergeOutcome};
use crate::execution::resubmit::{
    ResubmitRequest, ResubmitSender, MAX_RESUBMIT_ATTEMPTS, MIN_RESUBMIT_SHARES,
};
use crate::execution::signer::{Order, PolySigner};
use alloy_primitives::B256;
use crate::position::{ManagedOrder, OrderStatus, PositionSide};
use crate::strategy::decision::BuyIntent;
use crate::tui::app::AppState;
use crate::web::db::{DbMsg, DbSender, MyOrderRow};

const CLOB_HOST: &str = "https://clob.polymarket.com";
const ORDER_URL: &str = "https://clob.polymarket.com/order";
const ORDER_PATH: &str = "/order";
const DERIVE_API_KEY_URL: &str = "https://clob.polymarket.com/auth/derive-api-key";
const USER_AGENT: &str = "poly-btc-5m/0.4.5";

type HmacSha256 = Hmac<Sha256>;

#[inline]
fn unix_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[inline]
fn unix_ts_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[derive(Clone)]
pub struct ApiCreds {
    api_key_hv: HeaderValue,
    passphrase_hv: HeaderValue,
    hmac_template: HmacSha256,
}

impl ApiCreds {
    pub fn new(api_key: &str, secret_b64url: &str, passphrase: &str) -> Result<Self> {
        let decoded = URL_SAFE
            .decode(secret_b64url)
            .map_err(|e| anyhow!("base64 decode secret: {e}"))?;
        Ok(Self {
            hmac_template: HmacSha256::new_from_slice(&decoded)
                .map_err(|e| anyhow!("HMAC key: {e}"))?,
            api_key_hv: HeaderValue::from_str(api_key).map_err(|e| anyhow!("api_key: {e}"))?,
            passphrase_hv: HeaderValue::from_str(passphrase)
                .map_err(|e| anyhow!("passphrase: {e}"))?,
        })
    }

    fn sign(&self, ts: u64, method: &[u8], path: &[u8], body: &[u8]) -> String {
        let mut mac = self.hmac_template.clone();
        mac.update(ts.to_string().as_bytes());
        mac.update(method);
        mac.update(path);
        mac.update(body);
        URL_SAFE.encode(mac.finalize().into_bytes())
    }

    #[inline]
    pub fn sign_for_relayer(&self, ts: u64, method: &[u8], path: &[u8], body: &[u8]) -> String {
        self.sign(ts, method, path, body)
    }

    #[inline]
    pub fn api_key_header(&self) -> &HeaderValue {
        &self.api_key_hv
    }

    #[inline]
    pub fn passphrase_header(&self) -> &HeaderValue {
        &self.passphrase_hv
    }
}

pub fn build_http2_client() -> Result<Client> {
    Client::builder()
        .use_native_tls()
        .http2_prior_knowledge()
        .tcp_nodelay(true)
        .http2_adaptive_window(true)
        .http2_initial_stream_window_size(Some(4 * 1024 * 1024))
        .http2_initial_connection_window_size(Some(8 * 1024 * 1024))
        .pool_idle_timeout(Duration::from_secs(120))
        .timeout(Duration::from_secs(10))
        .connect_timeout(Duration::from_secs(3))
        .http2_keep_alive_interval(Duration::from_secs(20))
        .http2_keep_alive_while_idle(true)
        .http2_keep_alive_timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(USER_AGENT)
        .build()
        .map_err(anyhow::Error::from)
}

// GET /auth/derive-api-key。POLY_ADDRESS 用 EOA signer 地址（不是 Safe/Deposit maker）。
pub async fn derive_api_key(signer: &PolySigner, http: &Client) -> Result<(String, String, String)> {
    let address = signer.signer_address().to_checksum(None);
    let ts = unix_ts();
    let nonce: u64 = 0;

    let sig = signer.sign_clob_auth(ts, nonce).await?;
    let sig_hex = format!("0x{}", hex::encode(sig));

    let mut headers = HeaderMap::with_capacity(6);
    headers.insert("POLY_ADDRESS", HeaderValue::from_str(&address)?);
    headers.insert("POLY_SIGNATURE", HeaderValue::from_str(&sig_hex)?);
    headers.insert("POLY_TIMESTAMP", HeaderValue::from_str(&ts.to_string())?);
    headers.insert("POLY_NONCE", HeaderValue::from_str(&nonce.to_string())?);
    headers.insert("User-Agent", HeaderValue::from_static(USER_AGENT));
    headers.insert("Accept", HeaderValue::from_static("*/*"));

    let resp = http.get(DERIVE_API_KEY_URL).headers(headers).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("derive-api-key failed: {status} {body}");
    }
    let json: serde_json::Value = resp.json().await?;
    Ok((
        json["apiKey"].as_str().ok_or_else(|| anyhow!("missing apiKey"))?.to_string(),
        json["secret"].as_str().ok_or_else(|| anyhow!("missing secret"))?.to_string(),
        json["passphrase"].as_str().ok_or_else(|| anyhow!("missing passphrase"))?.to_string(),
    ))
}

/// 解析 CLOB POST /order 的成功回执。字段缺失全部按默认值兜底，
/// 因为 Polymarket 在 status=unmatched 时不会带 transactionsHashes / tradeIDs。
fn parse_order_response_json(text: &str, elapsed_ms: u64) -> Result<PlaceOrderResult> {
    let json: serde_json::Value = serde_json::from_str(text)?;
    let order_id = json["orderID"]
        .as_str()
        .ok_or_else(|| anyhow!("missing orderID: {text}"))?
        .to_string();
    let order_status = json["status"].as_str().unwrap_or("unknown").to_string();
    let making_amount = json["makingAmount"].as_str().and_then(|s| s.parse::<f64>().ok());
    let taking_amount = json["takingAmount"].as_str().and_then(|s| s.parse::<f64>().ok());
    let transactions_hashes = json["transactionsHashes"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let trade_ids = json["tradeIDs"]
        .as_array()
        .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    let success = json["success"].as_bool().unwrap_or(true);
    let error_msg = json["errorMsg"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from);

    debug!(%order_id, %order_status, "order placed");
    Ok(PlaceOrderResult {
        order_id,
        success,
        status: order_status,
        making_amount,
        taking_amount,
        transactions_hashes,
        trade_ids,
        error_msg,
        elapsed: elapsed_ms,
    })
}

#[inline]
fn extract_error_msg(text: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()?
        .get("errorMsg")?
        .as_str()
        .map(String::from)
}

pub fn to_json_v2(order: &Order, sig: &[u8], order_type: OrderType) -> serde_json::Value {
    let owner = order.maker.to_checksum(None);
    let mut p = json!({
        "owner": owner,
        "orderType": match order_type { OrderType::Fak => "FAK", OrderType::Gtc => "GTC" },
        "order": {
            "salt": order.salt.to_string(),
            "side": if order.side == 0 { "BUY" } else { "SELL" },
            "maker": owner,
            "signer": order.signer.to_checksum(None),
            "tokenId": order.tokenId.to_string(),
            "signature": format!("0x{}", hex::encode(sig)),
            "makerAmount": order.makerAmount.to_string(),
            "takerAmount": order.takerAmount.to_string(),
            "signatureType": order.signatureType,
            "timestamp": order.timestamp.to_string(),
            "metadata": format!("0x{}", hex::encode(order.metadata.as_slice())),
            "builder": format!("0x{}", hex::encode(order.builder.as_slice())),
        }
    });
    if matches!(order_type, OrderType::Gtc) {
        p["postOnly"] = serde_json::Value::Bool(false);
    }
    p
}

#[inline]
fn cancel_body(order_id: &str) -> Bytes {
    Bytes::from(format!(r#"{{"orderID":"{order_id}"}}"#))
}

// 预构建 POST/DELETE Request 模板：Accept / Content-Type / POLY_ADDRESS / POLY_API_KEY / POLY_PASSPHRASE
// 一次性设好，热路径只更新 POLY_TIMESTAMP / POLY_SIGNATURE / body。
fn build_request_template(
    url: &str,
    wallet: &str,
    creds: &ApiCreds,
    method: Method,
) -> Result<reqwest::Request> {
    let mut req = reqwest::Request::new(method, url.parse()?);
    let h = req.headers_mut();
    h.insert("Accept", HeaderValue::from_static("*/*"));
    h.insert("Content-Type", HeaderValue::from_static("application/json"));
    h.insert("POLY_ADDRESS", HeaderValue::from_str(wallet)?);
    h.insert("POLY_API_KEY", creds.api_key_hv.clone());
    h.insert("POLY_PASSPHRASE", creds.passphrase_hv.clone());
    Ok(req)
}

pub struct SharedClob {
    pub creds: ApiCreds,
    pub http: Client,
    pub state: Arc<RwLock<AppState>>,
    pub order_size_usdc: f64,
    pub maker_timeout_ms: i64,
    pub reprice_threshold: f64,
    pub db_tx: DbSender,
    pub resubmit_tx: Option<ResubmitSender>,
    pub post_template: reqwest::Request,
    pub signer: Arc<PolySigner>,
    pub circuit_breaker: Arc<CircuitBreaker>,
}

impl SharedClob {
    #[inline]
    fn stamp(&self, mut req: reqwest::Request, method: &str, path: &str, body: Bytes) -> reqwest::Request {
        let ts = unix_ts();
        let sig = self.creds.sign(ts, method.as_bytes(), path.as_bytes(), &body);
        let h = req.headers_mut();
        h.insert("POLY_TIMESTAMP", HeaderValue::from_str(&ts.to_string()).unwrap());
        h.insert("POLY_SIGNATURE", HeaderValue::from_str(&sig).unwrap());
        req.body_mut().replace(body.into());
        req
    }
}

pub struct LiveOrderClient {
    shared: Arc<SharedClob>,
    delete_template: reqwest::Request,
    merge_client: Option<Arc<MergeClient>>,
}

impl LiveOrderClient {
    pub fn attach_merge_client(&mut self, mc: Arc<MergeClient>) {
        self.merge_client = Some(mc);
    }

    pub fn attach_resubmit_tx(&mut self, tx: ResubmitSender) {
        // SAFETY: 启动时 LiveOrderClient 还没被 Arc 包装多份引用，&mut Arc::get_mut 可写入
        if let Some(s) = Arc::get_mut(&mut self.shared) {
            s.resubmit_tx = Some(tx);
        }
    }

    pub fn shared(&self) -> Arc<SharedClob> {
        Arc::clone(&self.shared)
    }
}

impl LiveOrderClient {
    pub async fn new(
        signer: PolySigner,
        creds: ApiCreds,
        order_size_usdc: f64,
        maker_timeout_ms: i64,
        state: Arc<RwLock<AppState>>,
        db_tx: DbSender,
        circuit_breaker: Arc<CircuitBreaker>,
    ) -> Result<Self> {
        Self::with_http(
            signer,
            creds,
            order_size_usdc,
            maker_timeout_ms,
            state,
            build_http2_client()?,
            db_tx,
            circuit_breaker,
        )
        .await
    }

    pub async fn with_http(
        signer: PolySigner,
        creds: ApiCreds,
        order_size_usdc: f64,
        maker_timeout_ms: i64,
        state: Arc<RwLock<AppState>>,
        http: Client,
        db_tx: DbSender,
        circuit_breaker: Arc<CircuitBreaker>,
    ) -> Result<Self> {
        let wallet = signer.maker().to_checksum(None);
        let post_template = build_request_template(ORDER_URL, &wallet, &creds, Method::POST)?;
        let delete_template = build_request_template(ORDER_URL, &wallet, &creds, Method::DELETE)?;

        // H2 warmup
        {
            let h = http.clone();
            tokio::spawn(async move {
                let t = Instant::now();
                if let Err(e) = h.get(CLOB_HOST).send().await {
                    warn!(error = %e, "H2 warmup failed");
                } else {
                    debug!(elapsed_us = ?t.elapsed(), "H2 warmup ok");
                }
            });
        }

        Ok(Self {
            shared: Arc::new(SharedClob {
                creds,
                http,
                state,
                order_size_usdc,
                maker_timeout_ms,
                reprice_threshold: 0.02,
                db_tx,
                resubmit_tx: None,
                post_template,
                signer: Arc::new(signer),
                circuit_breaker,
            }),
            delete_template,
            merge_client: None,
        })
    }
}

impl SharedClob {
    async fn post_order(&self, template: reqwest::Request, body: Bytes) -> Result<PlaceOrderResult> {
        let req = self.stamp(template, "POST", ORDER_PATH, body);
        let t = Instant::now();
        let resp = self.http.execute(req).await?;
        let ttfb = t.elapsed();
        let status = resp.status();
        let text = resp.text().await?;
        let body_read = t.elapsed() - ttfb;
        info!(ttfb_us = ?ttfb, body_read_us = ?body_read, total_us = ?t.elapsed(), "place_order timing");

        if status.as_u16() == 403 {
            anyhow::bail!("place order forbidden: {status}");
        }

        // 400 / 5xx：构造 success=false 的 PlaceOrderResult 而不是 bail，让上层入库 + resubmit。
        if !status.is_success() {
            debug!(%status, error = %text, "place_order failed");
            let err_msg = extract_error_msg(&text).unwrap_or_else(|| format!("({status}) {text}"));
            return Ok(PlaceOrderResult {
                success: false,
                status: format!("http_{}", status.as_u16()),
                error_msg: Some(err_msg),
                elapsed: t.elapsed().as_millis() as u64,
                ..Default::default()
            });
        }

        Ok(parse_order_response_json(&text, t.elapsed().as_millis() as u64)?)
    }

    /// 签名 + POST /order：resubmit worker 直接复用。
    pub async fn sign_and_post(&self, req: &PlaceOrderRequest) -> Result<PlaceOrderResult> {
        let salt: u64 = rand::random();
        let order = self.signer.build_order(req, salt, unix_ts_ms());
        let sig = self.signer.sign_order(&order).await?;
        let body = Bytes::from(serde_json::to_vec(&to_json_v2(&order, &sig, req.order_type))?);
        let template = clone_template(&self.post_template);
        self.post_order(template, body).await
    }

    async fn delete_order(&self, template: reqwest::Request, order_id: &str) -> Result<bool> {
        let req = self.stamp(template, "DELETE", ORDER_PATH, cancel_body(order_id));
        let resp = self.http.execute(req).await?;
        let status = resp.status();
        if status.is_success() || status == reqwest::StatusCode::NOT_FOUND {
            debug!(order_id, "cancel ok");
            Ok(true)
        } else {
            let text = resp.text().await.unwrap_or_default();
            warn!(order_id, %status, %text, "cancel rejected");
            Ok(false)
        }
    }
}

#[inline]
fn clone_template(t: &reqwest::Request) -> reqwest::Request {
    // Request 中只有 body 不可 clone；模板 body 始终为 None，try_clone 必成功。
    t.try_clone().expect("template body is None")
}

/// 下单瞬间冻结的"intent 侧"信息，用于 POST 异步回包后构造 my_orders 行。
#[derive(Debug, Clone)]
pub struct IntentSnapshot {
    pub client_order_id: String,
    pub side: PositionSide,
    pub outcome: &'static str,
    pub price: f64,
    pub size: f64,
    pub usd_value: f64,
    pub order_type: &'static str,
    pub token_id: String,
    pub condition_id: String,
    pub ts_ms: i64,
    pub window_end_ts: i64,
    pub reason: &'static str,
}

impl IntentSnapshot {
    #[allow(clippy::too_many_arguments)]
    fn for_dispatch(
        coid: &str,
        intent: &BuyIntent,
        token_id: &str,
        size_shares: f64,
        order_size_usdc: f64,
        order_type: OrderType,
        ts_ms: i64,
        state: &AppState,
    ) -> Self {
        Self {
            client_order_id: coid.to_string(),
            side: intent.side,
            outcome: match intent.side {
                PositionSide::Up => "up",
                PositionSide::Down => "down",
            },
            price: intent.target,
            size: size_shares,
            usd_value: order_size_usdc,
            order_type: match order_type {
                OrderType::Fak => "FAK",
                OrderType::Gtc => "GTC",
            },
            token_id: token_id.to_string(),
            condition_id: format!("0x{}", hex::encode(state.poly_condition_id.as_slice())),
            ts_ms,
            window_end_ts: state.poly_window_end_ts,
            reason: match intent.reason {
                crate::position::PendingOrderReason::Chase => "chase",
                crate::position::PendingOrderReason::Rebalance => "rebal",
            },
        }
    }
}

#[inline]
fn fmt_amount(v: Option<f64>) -> Option<String> {
    v.map(|x| format!("{x}"))
}

#[inline]
fn fmt_str_vec(v: &[String]) -> Option<String> {
    if v.is_empty() {
        None
    } else {
        serde_json::to_string(v).ok()
    }
}

pub fn build_order_row(
    intent: &IntentSnapshot,
    resp: &PlaceOrderResult,
    attempt: u8,
    parent_client_order_id: Option<String>,
) -> MyOrderRow {
    MyOrderRow {
        client_order_id: intent.client_order_id.clone(),
        parent_client_order_id,
        attempt,
        side: "buy",
        outcome: intent.outcome,
        price: intent.price,
        size: intent.size,
        usd_value: intent.usd_value,
        order_type: intent.order_type,
        token_id: intent.token_id.clone(),
        condition_id: intent.condition_id.clone(),
        ts_ms: intent.ts_ms,
        timestamp: None,
        window_end_ts: intent.window_end_ts,
        reason: intent.reason,
        success: resp.success,
        order_id: if resp.order_id.is_empty() {
            None
        } else {
            Some(resp.order_id.clone())
        },
        status: if resp.status.is_empty() {
            None
        } else {
            Some(resp.status.clone())
        },
        making_amount: fmt_amount(resp.making_amount),
        taking_amount: fmt_amount(resp.taking_amount),
        transactions_hashes_json: fmt_str_vec(&resp.transactions_hashes),
        trade_ids_json: fmt_str_vec(&resp.trade_ids),
        error_msg: resp.error_msg.clone(),
    }
}

/// 判定是否要进入 resubmit 链；满足则 send 到 shared.resubmit_tx。
pub fn maybe_resubmit(
    shared: &Arc<SharedClob>,
    intent: &IntentSnapshot,
    resp: &PlaceOrderResult,
    requested_size: f64,
    taking: f64,
    attempt: u8,
    parent_coid: &str,
) {
    let Some(tx) = shared.resubmit_tx.as_ref() else {
        return;
    };
    if attempt >= MAX_RESUBMIT_ATTEMPTS {
        return;
    }
    let (size_remaining, cumulative_filled) = if resp.success {
        // 200 OK：未达请求量 → 把剩余量丢去重发
        if taking <= 0.0 || taking >= requested_size {
            return;
        }
        (requested_size - taking, taking)
    } else {
        // 400 / 5xx：整笔重发
        (requested_size, 0.0)
    };
    if size_remaining < MIN_RESUBMIT_SHARES {
        return;
    }
    let next_coid = format!("{parent_coid}-r{}", attempt + 1);
    let req = ResubmitRequest {
        client_order_id: next_coid,
        parent_client_order_id: parent_coid.to_string(),
        token_id: intent.token_id.clone(),
        condition_id: intent.condition_id.clone(),
        side: intent.side,
        outcome: intent.outcome,
        max_price: intent.price, // 不加价
        size: size_remaining,
        cumulative_filled,
        original_size: requested_size,
        attempt: attempt + 1,
        ts_ms: intent.ts_ms,
        window_end_ts: intent.window_end_ts,
        reason: intent.reason,
    };
    let _ = tx.send(req);
}

#[async_trait]
impl OrderClient for LiveOrderClient {
    async fn place_order(&self, req: PlaceOrderRequest) -> Result<PlaceOrderResult> {
        let salt: u64 = rand::random();
        let order = self.shared.signer.build_order(&req, salt, unix_ts_ms());
        let sig = self.shared.signer.sign_order(&order).await?;
        let body = Bytes::from(serde_json::to_vec(&to_json_v2(&order, &sig, req.order_type))?);
        self.shared.post_order(clone_template(&self.shared.post_template), body).await
    }

    async fn cancel_order(&self, order_id: &str) -> Result<bool> {
        self.shared.delete_order(clone_template(&self.delete_template), order_id).await
    }

    async fn merge_pairs(&self, condition_id: B256, pair_qty: f64) -> Result<MergeOutcome> {
        let mc = self
            .merge_client
            .as_ref()
            .ok_or_else(|| anyhow!("LiveOrderClient: MergeClient 未挂载，启动期未注入"))?;
        mc.merge_pairs(condition_id, pair_qty).await
    }

    fn dispatch_buy_intent(
        &self,
        intent: &BuyIntent,
        best_ask: f64,
        token_id: &str,
        now_ms: i64,
        state: &mut AppState,
    ) {
        if state.ledger.has_open_buy_order(intent.side) || token_id.is_empty() || intent.target <= 0.0 {
            return;
        }
        let cash_pnl = state.cash_pnl();
        if let Err(reason) = self.shared.circuit_breaker.check(cash_pnl) {
            warn!(side = ?intent.side, %reason, "circuit breaker blocks dispatch_buy_intent");
            return;
        }
        let marketable = best_ask > 0.0 && best_ask <= intent.target;
        let order_type = if marketable { OrderType::Fak } else { OrderType::Gtc };
        let size_shares = self.shared.order_size_usdc / intent.target;

        let mut order = state.ledger.create_managed_buy_order(
            intent.side, intent.target, size_shares, now_ms, intent.reason,
        );
        order.target_price = intent.target;
        order.price = intent.target;
        order.maker_taker = !marketable;
        order.status = OrderStatus::Submitted;
        order.exchange_arrive_ts_ms = now_ms;
        order.updated_ts_ms = now_ms;
        let coid = order.client_order_id.clone();
        let intent_snap = IntentSnapshot::for_dispatch(
            &coid,
            intent,
            token_id,
            size_shares,
            self.shared.order_size_usdc,
            order_type,
            now_ms,
            state,
        );
        set_pending_order(state, intent.side, Some(order));

        let shared = self.shared.clone();
        let template = clone_template(&self.shared.post_template);
        let side = intent.side;
        let req = PlaceOrderRequest {
            side: OrderSide::Buy,
            token_id: token_id.to_string(),
            price: intent.target,
            size_shares,
            order_type,
        };
        let requested_size = size_shares;
        tokio::spawn(async move {
            let result: Result<PlaceOrderResult> = async {
                let salt: u64 = rand::random();
                let order = shared.signer.build_order(&req, salt, unix_ts_ms());
                let sig = shared.signer.sign_order(&order).await?;
                let body = Bytes::from(serde_json::to_vec(&to_json_v2(&order, &sig, req.order_type))?);
                shared.post_order(template, body).await
            }
            .await;
            let now = unix_ts_ms() as i64;
            match result {
                Ok(resp) => {
                    // 1. 落库
                    let row = build_order_row(&intent_snap, &resp, /* attempt */ 0, /* parent */ None);
                    let _ = shared.db_tx.send(DbMsg::OrderRow(row));

                    // 2. 推进 ledger 状态
                    let taking = resp.taking_amount.unwrap_or(0.0);
                    if resp.success {
                        if let Ok(mut s) = shared.state.write() {
                            if let Some(o) = pending_order_mut(&mut s, side) {
                                if o.client_order_id == coid {
                                    o.order_hash = Some(resp.order_id.clone());
                                    o.status = OrderStatus::Accepted;
                                    o.updated_ts_ms = now;
                                }
                            }
                        }
                        info!(?side, order_id = %resp.order_id, status = %resp.status, taking, "POST /order ok");
                        shared.circuit_breaker.record_success();
                    } else {
                        if let Ok(mut s) = shared.state.write() {
                            let ours = pending_order(&s, side).map(|o| o.client_order_id == coid).unwrap_or(false);
                            if ours {
                                if let Some(o) = pending_order_mut(&mut s, side) {
                                    o.mark_rejected(
                                        now,
                                        resp.error_msg.clone().unwrap_or_else(|| resp.status.clone()),
                                    );
                                }
                                archive_pending(&mut s, side);
                            }
                        }
                        warn!(?side, status = %resp.status, error = ?resp.error_msg, "POST /order rejected");
                        shared.circuit_breaker.record_error();
                    }

                    // 3. Resubmit 判定（200 部分成交 / 400 失败）
                    maybe_resubmit(&shared, &intent_snap, &resp, requested_size, taking, 0, &coid);
                }
                Err(e) => {
                    if let Ok(mut s) = shared.state.write() {
                        let ours = pending_order(&s, side).map(|o| o.client_order_id == coid).unwrap_or(false);
                        if ours {
                            if let Some(o) = pending_order_mut(&mut s, side) {
                                o.mark_rejected(now, format!("post_failed: {e}"));
                            }
                            archive_pending(&mut s, side);
                        }
                    }
                    warn!(?side, error = %e, "POST /order transport failed (no DB row written)");
                    shared.circuit_breaker.record_error();
                }
            }
        });
    }

    fn tick(
        &self,
        state: &mut AppState,
        target_up: f64,
        target_down: f64,
        _force_rebalance_taker: bool,
        now_ms: i64,
    ) -> (bool, bool) {
        for (side, target) in [(PositionSide::Up, target_up), (PositionSide::Down, target_down)] {
            let Some(order) = pending_order(state, side) else { continue };
            if order.status == OrderStatus::CancelRequested {
                continue;
            }
            let Some(oid) = order.order_hash.clone() else {
                continue;
            };

            let ttl = now_ms - order.placed_ts_ms > self.shared.maker_timeout_ms;
            let reprice = target > 0.0 && (target - order.price).abs() >= self.shared.reprice_threshold;
            if !ttl && !reprice {
                continue;
            }

            if let Some(o) = pending_order_mut(state, side) {
                o.mark_cancel_requested(now_ms);
                o.reject_reason = Some(if ttl { "ttl_expired" } else { "reprice" }.to_string());
            }

            let shared = self.shared.clone();
            let template = clone_template(&self.delete_template);
            tokio::spawn(async move {
                let now = unix_ts_ms() as i64;
                match shared.delete_order(template, &oid).await {
                    Ok(true) => {
                        if let Ok(mut s) = shared.state.write() {
                            if let Some(o) = pending_order_mut(&mut s, side) {
                                if o.order_hash.as_deref() == Some(&oid) {
                                    o.mark_cancelled(now);
                                }
                            }
                            archive_pending(&mut s, side);
                        }
                    }
                    Ok(false) => {}
                    Err(e) => warn!(?side, order_id = %oid, error = %e, "DELETE /order failed"),
                }
            });
        }
        (false, false)
    }
}

#[inline]
fn pending_order(s: &AppState, side: PositionSide) -> Option<&ManagedOrder> {
    match side {
        PositionSide::Up => s.ledger.maker_buy_intent_up.as_ref(),
        PositionSide::Down => s.ledger.maker_buy_intent_down.as_ref(),
    }
}

#[inline]
fn pending_order_mut(s: &mut AppState, side: PositionSide) -> Option<&mut ManagedOrder> {
    match side {
        PositionSide::Up => s.ledger.maker_buy_intent_up.as_mut(),
        PositionSide::Down => s.ledger.maker_buy_intent_down.as_mut(),
    }
}

#[inline]
fn set_pending_order(s: &mut AppState, side: PositionSide, order: Option<ManagedOrder>) {
    match side {
        PositionSide::Up => s.ledger.maker_buy_intent_up = order,
        PositionSide::Down => s.ledger.maker_buy_intent_down = order,
    }
}

#[inline]
fn archive_pending(s: &mut AppState, side: PositionSide) {
    let slot = match side {
        PositionSide::Up => &mut s.ledger.maker_buy_intent_up,
        PositionSide::Down => &mut s.ledger.maker_buy_intent_down,
    };
    if let Some(o) = slot.take() {
        s.ledger.order_history.push(o);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, Address, B256, U256};

    // HMAC 鉴权契约：固定输入 → 固定 base64url 签名。
    #[test]
    fn hmac_sign_known_vector() {
        let creds = ApiCreds::new("k", "dGVzdA==", "p").unwrap();
        let sig = creds.sign(1_700_000_000, b"POST", b"/order", br#"{"a":1}"#);
        assert_eq!(sig, "DJzQx-noUj0VaMJ2f_RVnVzasXvp8zZw9oEPFTm-26o=");
    }

    // V2 JSON 形态契约：GTC 含 postOnly=false、FAK 无 postOnly；地址走 EIP-55 checksum。
    #[test]
    fn to_json_v2_shape_invariants() {
        const EOA: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
        const WALLET: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let order = Order {
            salt: U256::from(0xC0FFEE_u64),
            maker: WALLET,
            signer: EOA,
            tokenId: U256::from(1234567890_u64),
            makerAmount: U256::from(42_000_000_u64),
            takerAmount: U256::from(100_000_000_u64),
            side: 0,
            signatureType: 2,
            timestamp: U256::from(1_700_000_000_000_u64),
            metadata: B256::ZERO,
            builder: B256::ZERO,
        };

        let fak = to_json_v2(&order, b"\x00", OrderType::Fak);
        assert_eq!(fak["orderType"], "FAK");
        assert!(fak.get("postOnly").is_none());

        let gtc = to_json_v2(&order, b"\x00", OrderType::Gtc);
        assert_eq!(gtc["orderType"], "GTC");
        assert_eq!(gtc["postOnly"], false);
        assert_eq!(gtc["order"]["signer"].as_str().unwrap(), EOA.to_checksum(None));
    }
}
