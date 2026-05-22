//! FAK 部分成交 / 400 失败的重发链。
//!
//! 设计参考 `polymarket-copytrader-main/src/main.rs:574-720` 的 resubmit_worker：
//!   * 一条 mpsc 链：dispatch_buy_intent → resubmit_tx → run_resubmit_worker
//!   * 每次重发自己也判定是否要再续一环（self_tx），attempt 上限 4
//!   * 5m BTC 小额场景：整链不加价（`max_price` 始终 = 原 target）
//!
//! 不加价 tier 简化为常量；要分层调价时在 worker 入口加 tier 分发。

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::execution::client::{OrderSide, OrderType, PlaceOrderRequest};
use crate::execution::transaction::{build_order_row, IntentSnapshot, SharedClob};
use crate::position::PositionSide;
use crate::web::db::DbMsg;

/// 上限：首单 + 4 次重发 = 共 5 次尝试。
pub const MAX_RESUBMIT_ATTEMPTS: u8 = 4;

/// 单次重发的最小 size 阈值（shares）。低于则放弃，避免精度噪声 / 手续费白白浪费。
pub const MIN_RESUBMIT_SHARES: f64 = 1.0;

/// 重发节流。
pub const RESUBMIT_SPACING_MS: u64 = 50;

#[derive(Debug, Clone)]
pub struct ResubmitRequest {
    /// 本次重发新分配的 client_order_id（与 my_orders 主表对齐）
    pub client_order_id: String,
    /// 上一环的 client_order_id（首次重发 = 原首单的 coid）
    pub parent_client_order_id: String,
    pub token_id: String,
    pub condition_id: String,
    pub side: PositionSide,
    pub outcome: &'static str,
    pub max_price: f64,
    pub size: f64,
    pub cumulative_filled: f64,
    pub original_size: f64,
    pub attempt: u8,
    pub ts_ms: i64,
    pub window_end_ts: i64,
    pub reason: &'static str,
}

pub type ResubmitSender = mpsc::UnboundedSender<ResubmitRequest>;
pub type ResubmitReceiver = mpsc::UnboundedReceiver<ResubmitRequest>;

/// 取出 `req` 中 IntentSnapshot 投回 my_orders 用的视图。
fn snapshot_from(req: &ResubmitRequest) -> IntentSnapshot {
    IntentSnapshot {
        client_order_id: req.client_order_id.clone(),
        side: req.side,
        outcome: req.outcome,
        price: req.max_price,
        size: req.size,
        usd_value: req.size * req.max_price,
        order_type: "FAK",
        token_id: req.token_id.clone(),
        condition_id: req.condition_id.clone(),
        ts_ms: req.ts_ms,
        window_end_ts: req.window_end_ts,
        reason: req.reason,
    }
}

pub async fn run_resubmit_worker(
    mut rx: ResubmitReceiver,
    shared: Arc<SharedClob>,
    self_tx: ResubmitSender,
) {
    info!("resubmit_worker started (max_attempts={})", MAX_RESUBMIT_ATTEMPTS);
    while let Some(req) = rx.recv().await {
        // 节流：避免连续重发被风控/打挂
        tokio::time::sleep(Duration::from_millis(RESUBMIT_SPACING_MS)).await;

        let place_req = PlaceOrderRequest {
            side: OrderSide::Buy,
            token_id: req.token_id.clone(),
            price: req.max_price,
            size_shares: req.size,
            order_type: OrderType::Fak, // resubmit 一律走 FAK（GTC 没必要 chase）
        };
        let attempt = req.attempt;
        let parent_coid = req.parent_client_order_id.clone();
        let requested_size = req.size;
        let intent_snap = snapshot_from(&req);

        match shared.sign_and_post(&place_req).await {
            Ok(resp) => {
                let taking = resp.taking_amount.unwrap_or(0.0);
                let row = build_order_row(
                    &intent_snap,
                    &resp,
                    attempt,
                    Some(parent_coid.clone()),
                );
                let _ = shared.db_tx.send(DbMsg::OrderRow(row));
                info!(
                    side = ?req.side,
                    attempt,
                    order_id = %resp.order_id,
                    status = %resp.status,
                    taking,
                    "resubmit POST ok"
                );
                // 链式判定（resubmit_tx 用 self_tx 而非 shared 内的，避免循环依赖）
                if attempt < MAX_RESUBMIT_ATTEMPTS {
                    if let Some(next) = build_next_request(&req, &resp, requested_size, taking) {
                        let _ = self_tx.send(next);
                    }
                }
            }
            Err(e) => {
                warn!(side = ?req.side, attempt, error = %e, "resubmit POST transport failed");
            }
        }
    }
    warn!("resubmit_worker exit (channel closed)");
}

/// 复用 `maybe_resubmit` 的判定逻辑但产出 ResubmitRequest 而非 send；这里手动构造以便控制 self_tx。
fn build_next_request(
    prev: &ResubmitRequest,
    resp: &crate::execution::client::PlaceOrderResult,
    requested_size: f64,
    taking: f64,
) -> Option<ResubmitRequest> {
    let (size_remaining, cumulative_filled) = if resp.success {
        if taking <= 0.0 || taking >= requested_size {
            return None;
        }
        (requested_size - taking, prev.cumulative_filled + taking)
    } else {
        (requested_size, prev.cumulative_filled)
    };
    if size_remaining < MIN_RESUBMIT_SHARES {
        return None;
    }
    let next_attempt = prev.attempt + 1;
    Some(ResubmitRequest {
        client_order_id: format!("{}-r{}", base_coid(&prev.client_order_id), next_attempt),
        parent_client_order_id: prev.client_order_id.clone(),
        token_id: prev.token_id.clone(),
        condition_id: prev.condition_id.clone(),
        side: prev.side,
        outcome: prev.outcome,
        max_price: prev.max_price, // 不加价
        size: size_remaining,
        cumulative_filled,
        original_size: prev.original_size,
        attempt: next_attempt,
        ts_ms: prev.ts_ms,
        window_end_ts: prev.window_end_ts,
        reason: prev.reason,
    })
}

/// "abc-r1" → "abc"，避免 "abc-r1-r2" 这种叠后缀。
#[inline]
fn base_coid(s: &str) -> &str {
    s.rsplit_once("-r").map(|(base, _)| base).unwrap_or(s)
}

