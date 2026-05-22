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

use crate::execution::client::{
    BuyIntent, OrderClient, OrderSide, OrderType, PlaceOrderRequest, PlaceOrderResult,
};
use crate::execution::signer::{Order, PolySigner};
use crate::position::{ManagedOrder, OrderStatus, PendingOrderReason, PositionSide};
use crate::tui::app::AppState;

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

// GET /auth/derive-api-key. POLY_ADDRESS uses the EOA signer address (not Safe/Deposit maker).
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

// Pre-built POST/DELETE Request template: Accept / Content-Type / POLY_ADDRESS / POLY_API_KEY /
// POLY_PASSPHRASE are set once; the hot path only updates POLY_TIMESTAMP / POLY_SIGNATURE / body.
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

struct SharedClob {
    creds: ApiCreds,
    http: Client,
    state: Arc<RwLock<AppState>>,
    order_size_usdc: f64,
    maker_timeout_ms: i64,
    reprice_threshold: f64,
}

impl SharedClob {
    #[inline]
    fn stamp(
        &self,
        mut req: reqwest::Request,
        method: &str,
        path: &str,
        body: Bytes,
    ) -> reqwest::Request {
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
    signer: Arc<PolySigner>,
    shared: Arc<SharedClob>,
    post_template: reqwest::Request,
    delete_template: reqwest::Request,
}

impl LiveOrderClient {
    pub async fn new(
        signer: PolySigner,
        creds: ApiCreds,
        order_size_usdc: f64,
        maker_timeout_ms: i64,
        state: Arc<RwLock<AppState>>,
    ) -> Result<Self> {
        Self::with_http(
            signer,
            creds,
            order_size_usdc,
            maker_timeout_ms,
            state,
            build_http2_client()?,
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
            signer: Arc::new(signer),
            shared: Arc::new(SharedClob {
                creds,
                http,
                state,
                order_size_usdc,
                maker_timeout_ms,
                reprice_threshold: 0.02,
            }),
            post_template,
            delete_template,
        })
    }
}

impl SharedClob {
    async fn post_order(
        &self,
        template: reqwest::Request,
        body: Bytes,
    ) -> Result<PlaceOrderResult> {
        let req = self.stamp(template, "POST", ORDER_PATH, body);
        let t = Instant::now();
        let resp = self.http.execute(req).await?;
        let ttfb = t.elapsed();
        let status = resp.status();
        let mut text = resp.text().await?;
        let body_read = t.elapsed() - ttfb;
        info!(
            ttfb_us = ?ttfb,
            body_read_us = ?body_read,
            total_us = ?t.elapsed(),
            "place_order timing"
        );

        if status.as_u16() == 403 {
            anyhow::bail!("place order forbidden: {status}");
        }
        if !status.is_success() {
            debug!(%status, error = %text, "place_order failed");
            anyhow::bail!("({status}) {text}");
        }

        let json: serde_json::Value = serde_json::from_str(&text)?;
        let order_id = json["orderID"]
            .as_str()
            .ok_or_else(|| anyhow!("missing orderID: {text}"))?
            .to_string();
        text.clear();
        let order_status = json["status"].as_str().unwrap_or("unknown").to_string();
        let filled_size = json["takingAmount"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok());

        debug!(%order_id, %order_status, "order placed");
        Ok(PlaceOrderResult {
            order_id,
            success: true,
            status: order_status,
            filled_price: None,
            filled_size,
            error: None,
            elapsed: t.elapsed().as_millis() as u64,
        })
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
    // Only Request body is non-clonable; templates always have body=None, so try_clone never fails.
    t.try_clone().expect("template body is None")
}

#[async_trait]
impl OrderClient for LiveOrderClient {
    async fn place_order(&self, req: PlaceOrderRequest) -> Result<PlaceOrderResult> {
        let salt: u64 = rand::random();
        let order = self.signer.build_order(&req, salt, unix_ts_ms());
        let sig = self.signer.sign_order(&order).await?;
        let body = Bytes::from(serde_json::to_vec(&to_json_v2(&order, &sig, req.order_type))?);
        self.shared
            .post_order(clone_template(&self.post_template), body)
            .await
    }

    async fn cancel_order(&self, order_id: &str) -> Result<bool> {
        self.shared
            .delete_order(clone_template(&self.delete_template), order_id)
            .await
    }

    fn dispatch_buy_intent(
        &self,
        intent: &BuyIntent,
        best_ask: f64,
        token_id: &str,
        now_ms: i64,
        state: &mut AppState,
    ) {
        if pending_order(state, intent.side).is_some()
            || token_id.is_empty()
            || intent.target <= 0.0
        {
            return;
        }
        let marketable = best_ask > 0.0 && best_ask <= intent.target;
        let order_type = if marketable { OrderType::Fak } else { OrderType::Gtc };
        let size_shares = self.shared.order_size_usdc / intent.target;

        let reason = intent.reason;
        let mut order =
            ManagedOrder::new_buy(intent.side, intent.target, size_shares, now_ms, reason);
        order.target_price = intent.target;
        order.price = intent.target;
        order.maker_taker = !marketable;
        order.status = OrderStatus::Submitted;
        order.exchange_arrive_ts_ms = now_ms;
        order.updated_ts_ms = now_ms;
        let coid = order.client_order_id.clone();
        set_pending_order(state, intent.side, Some(order));

        let signer = self.signer.clone();
        let shared = self.shared.clone();
        let template = clone_template(&self.post_template);
        let side = intent.side;
        let req = PlaceOrderRequest {
            side: OrderSide::Buy,
            token_id: token_id.to_string(),
            price: intent.target,
            size_shares,
            order_type,
        };
        let _ = reason;
        tokio::spawn(async move {
            let result: Result<PlaceOrderResult> = async {
                let salt: u64 = rand::random();
                let order = signer.build_order(&req, salt, unix_ts_ms());
                let sig = signer.sign_order(&order).await?;
                let body = Bytes::from(serde_json::to_vec(&to_json_v2(&order, &sig, req.order_type))?);
                shared.post_order(template, body).await
            }
            .await;
            let now = unix_ts_ms() as i64;
            match result {
                Ok(resp) => {
                    if let Ok(mut s) = shared.state.write() {
                        if let Some(o) = pending_order_mut(&mut s, side) {
                            if o.client_order_id == coid {
                                o.order_hash = Some(resp.order_id.clone());
                                o.status = OrderStatus::Accepted;
                                o.updated_ts_ms = now;
                            }
                        }
                    }
                    info!(?side, order_id = %resp.order_id, elapsed = resp.elapsed, "POST /order ok");
                }
                Err(e) => {
                    if let Ok(mut s) = shared.state.write() {
                        let ours = pending_order(&s, side)
                            .map(|o| o.client_order_id == coid)
                            .unwrap_or(false);
                        if ours {
                            if let Some(o) = pending_order_mut(&mut s, side) {
                                o.mark_rejected(now, format!("post_failed: {e}"));
                            }
                            clear_pending(&mut s, side);
                        }
                    }
                    warn!(?side, error = %e, "POST /order failed");
                }
            }
        });
    }

    fn tick(
        &self,
        state: &mut AppState,
        target_up: f64,
        target_down: f64,
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
            let reprice =
                target > 0.0 && (target - order.price).abs() >= self.shared.reprice_threshold;
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
                            clear_pending(&mut s, side);
                        }
                    }
                    Ok(false) => {}
                    Err(e) => {
                        warn!(?side, order_id = %oid, error = %e, "DELETE /order failed")
                    }
                }
            });
        }
        (false, false)
    }
}

// Reason kept on ManagedOrder so signal.rs / TUI can introspect, but transaction.rs uses it as
// opaque metadata; reference suppresses unused warning.
#[allow(dead_code)]
fn _reason_marker(_r: PendingOrderReason) {}

#[inline]
fn pending_order(s: &AppState, side: PositionSide) -> Option<&ManagedOrder> {
    match side {
        PositionSide::Up => s.pending_order_up.as_ref(),
        PositionSide::Down => s.pending_order_down.as_ref(),
    }
}

#[inline]
fn pending_order_mut(s: &mut AppState, side: PositionSide) -> Option<&mut ManagedOrder> {
    match side {
        PositionSide::Up => s.pending_order_up.as_mut(),
        PositionSide::Down => s.pending_order_down.as_mut(),
    }
}

#[inline]
fn set_pending_order(s: &mut AppState, side: PositionSide, order: Option<ManagedOrder>) {
    match side {
        PositionSide::Up => s.pending_order_up = order,
        PositionSide::Down => s.pending_order_down = order,
    }
}

#[inline]
fn clear_pending(s: &mut AppState, side: PositionSide) {
    match side {
        PositionSide::Up => s.pending_order_up = None,
        PositionSide::Down => s.pending_order_down = None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{address, Address, B256, U256};

    #[test]
    fn hmac_sign_known_vector() {
        let creds = ApiCreds::new("k", "dGVzdA==", "p").unwrap();
        let sig = creds.sign(1_700_000_000, b"POST", b"/order", br#"{"a":1}"#);
        assert_eq!(sig, "DJzQx-noUj0VaMJ2f_RVnVzasXvp8zZw9oEPFTm-26o=");
    }

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
