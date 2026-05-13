use chrono::TimeZone;
use std::io::{self, Write};
use std::path::Path;

/// 当前可追涨的一侧（UP/DOWN 不会同时涨）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PositionSide {
    Up,
    Down,
}

fn side_label(side: PositionSide) -> &'static str {
    match side {
        PositionSide::Up => "UP",
        PositionSide::Down => "DOWN",
    }
}

/// 当前 pending 买单来源。区分追涨建仓和偏仓配平，便于后续独立统计与风控。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingOrderReason {
    Chase,
    Rebalance,
}

fn reason_label(reason: PendingOrderReason) -> &'static str {
    match reason {
        PendingOrderReason::Chase => "chase",
        PendingOrderReason::Rebalance => "rebal",
    }
}

/// 订单生命周期状态。实盘时仓位只应由真实 fill 回报推进。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderStatus {
    /// 本地已创建，尚未提交到交易所。dry-run maker 意图也停留在这个状态。
    Created,
    /// 已发出提交请求，等待交易所确认。
    Submitted,
    /// 交易所已接受，等待成交/撤单。
    Accepted,
    /// 已部分成交，仍有剩余数量暴露。
    PartiallyFilled,
    /// 已完全成交。
    Filled,
    /// 已发出撤单请求，但尚未确认撤单成功；风险仍然存在。
    CancelRequested,
    /// 已确认撤单。
    Cancelled,
    /// 下单被拒绝。
    Rejected,
    /// 本地超时/过期。
    Expired,
}

impl OrderStatus {
    pub fn is_open(self) -> bool {
        matches!(
            self,
            OrderStatus::Created
                | OrderStatus::Submitted
                | OrderStatus::Accepted
                | OrderStatus::PartiallyFilled
                | OrderStatus::CancelRequested
        )
    }
}

fn status_label(status: OrderStatus) -> &'static str {
    match status {
        OrderStatus::Created => "created",
        OrderStatus::Submitted => "submitted",
        OrderStatus::Accepted => "accepted",
        OrderStatus::PartiallyFilled => "partial",
        OrderStatus::Filled => "filled",
        OrderStatus::CancelRequested => "cancel_requested",
        OrderStatus::Cancelled => "cancelled",
        OrderStatus::Rejected => "rejected",
        OrderStatus::Expired => "expired",
    }
}

/// 本地托管订单。当前 dry-run 只使用 Created/Filled/Expired；实盘接入后填充 order_hash 并按回报推进状态。
#[derive(Clone, Debug)]
pub struct ManagedOrder {
    pub order_id: u64,
    pub client_order_id: String,
    pub order_hash: Option<String>,
    pub side: PositionSide,
    pub buy_sell: bool,
    pub maker_taker: bool, // true=maker, false=taker
    /// 策略目标价（chase = FV - safety，rebal = 同侧 target），用于和 worst_price 对比评估滑点预算。
    pub target_price: f64,
    /// Execution limit price used by taker FAK and one-shot maker probes.
    pub price: f64,
    pub qty: f64,
    pub filled_qty: f64,
    /// fill 累计 notional = Σ(price_i × qty_i)，配合 filled_qty 算 VWAP
    pub filled_notional: f64,
    /// 实际吃到的盘口档数；0 表示未成交，1 表示只吃 best ask。
    pub fill_levels: u32,
    pub placed_ts_ms: i64,
    pub updated_ts_ms: i64,
    pub exchange_arrive_ts_ms: i64,
    pub queue_ahead_qty: f64,
    pub reason: PendingOrderReason,
    pub status: OrderStatus,
    pub reject_reason: Option<String>,
    /// net_pnl snapshot at the moment this order was last updated (fill/cancel/reject).
    pub snapshot_pnl: f64,
}

impl ManagedOrder {
    pub fn new(
        order_id: u64,
        side: PositionSide,
        price: f64,
        qty: f64,
        placed_ts_ms: i64,
        reason: PendingOrderReason,
    ) -> Self {
        Self {
            order_id,
            client_order_id: format!("dry-{}-{}-{}", placed_ts_ms, side_label(side), order_id),
            order_hash: None,
            side,
            buy_sell: true,
            maker_taker: true,
            target_price: price,
            price,
            qty,
            filled_qty: 0.0,
            filled_notional: 0.0,
            fill_levels: 0,
            placed_ts_ms,
            updated_ts_ms: placed_ts_ms,
            exchange_arrive_ts_ms: placed_ts_ms,
            queue_ahead_qty: 0.0,
            reason,
            status: OrderStatus::Created,
            reject_reason: None,
            snapshot_pnl: 0.0,
        }
    }

    pub fn remaining_qty(&self) -> f64 {
        (self.qty - self.filled_qty).max(0.0)
    }

    pub fn is_open(&self) -> bool {
        self.status.is_open() && self.remaining_qty() > 0.0
    }

    pub fn mark_expired(&mut self, ts_ms: i64) {
        self.status = OrderStatus::Expired;
        self.updated_ts_ms = ts_ms;
    }

    pub fn mark_cancel_requested(&mut self, ts_ms: i64) {
        self.status = OrderStatus::CancelRequested;
        self.updated_ts_ms = ts_ms;
    }

    pub fn mark_cancelled(&mut self, ts_ms: i64) {
        self.status = OrderStatus::Cancelled;
        self.updated_ts_ms = ts_ms;
    }

    pub fn mark_rejected(&mut self, ts_ms: i64, reason: impl Into<String>) {
        self.status = OrderStatus::Rejected;
        self.reject_reason = Some(reason.into());
        self.updated_ts_ms = ts_ms;
    }

    pub fn record_fill(&mut self, fill_qty: f64, ts_ms: i64) {
        self.filled_qty = (self.filled_qty + fill_qty).min(self.qty);
        self.status = if self.remaining_qty() > 0.0 {
            OrderStatus::PartiallyFilled
        } else {
            OrderStatus::Filled
        };
        self.updated_ts_ms = ts_ms;
    }

    /// 记录某档 fill 的实际成交价，累计 notional 用于 VWAP
    pub fn record_fill_at(&mut self, fill_qty: f64, fill_price: f64, ts_ms: i64) {
        self.filled_notional += fill_qty * fill_price;
        self.fill_levels = self.fill_levels.saturating_add(1);
        self.record_fill(fill_qty, ts_ms);
    }

    /// VWAP；filled_qty=0 时返回 0
    pub fn vwap(&self) -> f64 {
        if self.filled_qty > 0.0 {
            self.filled_notional / self.filled_qty
        } else {
            0.0
        }
    }
}

/// 单笔成交记录（用于落盘与统计）；成交瞬间的盘口为快照，不事后计算
#[derive(Clone, Debug)]
pub struct TradeRecord {
    pub side: PositionSide,
    pub buy_sell: bool,    // true=buy, false=sell
    pub maker_taker: bool, // true=maker, false=taker
    pub price: f64,
    pub qty: f64,
    pub ts_ms: i64,
    pub window_end_ts: i64,
    pub up_ask: f64,
    pub up_bid: f64,
    pub up_spread: f64,
    pub down_ask: f64,
    pub down_bid: f64,
    pub down_spread: f64,
    /// net_pnl snapshot at the moment this fill occurred.
    pub snapshot_pnl: f64,
}

/// 单侧仓位（数量 + 持仓均价）
#[derive(Clone, Debug, Default)]
pub struct Position {
    pub qty: f64,
    pub avg_price: f64,
}

impl Position {
    pub fn cost(&self) -> f64 {
        self.qty * self.avg_price
    }

    /// 浮盈/浮亏金额（多仓：(now - avg)*qty）
    pub fn float_pnl(&self, price_now: f64) -> f64 {
        (price_now - self.avg_price) * self.qty
    }

    /// 浮亏% = (avg - now)/avg（仅当 avg > 0 且亏损时有意义）
    pub fn float_loss_pct(&self, price_now: f64) -> Option<f64> {
        if self.avg_price <= 0.0 || self.qty <= 0.0 {
            return None;
        }
        if price_now >= self.avg_price {
            return Some(0.0);
        }
        Some((self.avg_price - price_now) / self.avg_price)
    }
}

/// 仓位/P&L 账本。后续真实下单接入时，只有成交、撤单、merge/redeem 回报应写入这里。
#[derive(Clone, Debug, Default)]
pub struct PositionLedger {
    pub position_up: Position,
    pub position_down: Position,
    /// 本窗口内成交记录（市场切换时落盘）
    pub trades_current_window: Vec<TradeRecord>,
    /// 已发生 taker 手续费累计（公式：qty × 0.072 × p × (1−p)）
    pub total_fee: f64,
    /// Maker rebate 累计（25% 的 taker fee 返还给做市商）
    pub total_rebate: f64,
    /// 已实现盈亏累计（旧 sell 路径保留字段）
    pub realized_pnl: f64,
    /// UP 侧托管买单（dry-run 意图；实盘接入后由订单回报驱动状态变化）
    pub maker_buy_intent_up: Option<ManagedOrder>,
    /// DOWN 侧托管买单（dry-run 意图；实盘接入后由订单回报驱动状态变化）
    pub maker_buy_intent_down: Option<ManagedOrder>,
    /// 已终结订单历史（filled/cancelled/rejected/expired），用于审计与后续实盘回放。
    pub order_history: Vec<ManagedOrder>,
    /// 本地订单序号，生成 client_order_id 并关联 dry-run 状态。
    pub next_order_id: u64,
    /// 累计已虚拟 merge 的对数（每对兑换 1.00 USDC）
    pub merged_pairs: f64,
    /// 累计 apply_merge 被实际执行的次数（pq>0 才计）；用于区分"对数"与"调用次数"
    pub merge_count: u64,
    /// Merge 实现盈亏 = Σ pair_qty × (1.00 - (avg_up + avg_down))
    pub merge_pnl: f64,
    /// Merge 收到的现金（USDC）= merged_pairs × 1.00
    pub cash_received: f64,
    /// Buy 累计花出去的现金（USDC）= Σ buy_price × buy_qty
    pub cash_paid: f64,
    /// 上次 merge 时间戳（ms），用于节流
    pub last_merge_ts_ms: i64,
    /// 配平提示 UP：(配平需多少张, 若全配平价后均价之和)
    pub rebalance_hint_up: Option<(f64, f64)>,
    /// 配平提示 DOWN：(配平需多少张, 若全配平价后均价之和)
    pub rebalance_hint_down: Option<(f64, f64)>,
}

impl PositionLedger {
    pub fn position_avg_sum(&self) -> f64 {
        self.position_up.avg_price + self.position_down.avg_price
    }

    pub fn projected_qty_up_after_intents(&self) -> f64 {
        self.position_up.qty
            + self
                .maker_buy_intent_up
                .as_ref()
                .filter(|o| o.is_open())
                .map(|o| o.remaining_qty())
                .unwrap_or(0.0)
    }

    pub fn projected_qty_down_after_intents(&self) -> f64 {
        self.position_down.qty
            + self
                .maker_buy_intent_down
                .as_ref()
                .filter(|o| o.is_open())
                .map(|o| o.remaining_qty())
                .unwrap_or(0.0)
    }

    pub fn projected_avg_sum_after_intents(&self) -> f64 {
        let proj_avg_up = match self.maker_buy_intent_up.as_ref() {
            Some(o) if o.is_open() => weighted_avg(
                self.position_up.qty,
                self.position_up.avg_price,
                o.remaining_qty(),
                o.price,
            ),
            None => self.position_up.avg_price,
            Some(_) => self.position_up.avg_price,
        };
        let proj_avg_down = match self.maker_buy_intent_down.as_ref() {
            Some(o) if o.is_open() => weighted_avg(
                self.position_down.qty,
                self.position_down.avg_price,
                o.remaining_qty(),
                o.price,
            ),
            None => self.position_down.avg_price,
            Some(_) => self.position_down.avg_price,
        };
        proj_avg_up + proj_avg_down
    }

    pub fn create_managed_buy_order(
        &mut self,
        side: PositionSide,
        price: f64,
        qty: f64,
        placed_ts_ms: i64,
        reason: PendingOrderReason,
    ) -> ManagedOrder {
        self.next_order_id = self.next_order_id.saturating_add(1);
        ManagedOrder::new(self.next_order_id, side, price, qty, placed_ts_ms, reason)
    }

    pub fn has_open_buy_order(&self, side: PositionSide) -> bool {
        match side {
            PositionSide::Up => self.maker_buy_intent_up.as_ref(),
            PositionSide::Down => self.maker_buy_intent_down.as_ref(),
        }
        .map(|o| o.is_open())
        .unwrap_or(false)
    }

    pub fn archive_filled_buy_order(&mut self, side: PositionSide, fill_qty: f64, ts_ms: i64) {
        let slot = match side {
            PositionSide::Up => &mut self.maker_buy_intent_up,
            PositionSide::Down => &mut self.maker_buy_intent_down,
        };
        if let Some(mut order) = slot.take() {
            order.record_fill(fill_qty, ts_ms);
            self.order_history.push(order);
        }
    }

    pub fn expire_buy_order(&mut self, side: PositionSide, ts_ms: i64) {
        let slot = match side {
            PositionSide::Up => &mut self.maker_buy_intent_up,
            PositionSide::Down => &mut self.maker_buy_intent_down,
        };
        if let Some(mut order) = slot.take() {
            order.mark_expired(ts_ms);
            self.order_history.push(order);
        }
    }

    pub fn cancel_buy_order_local(&mut self, side: PositionSide, ts_ms: i64) {
        let slot = match side {
            PositionSide::Up => &mut self.maker_buy_intent_up,
            PositionSide::Down => &mut self.maker_buy_intent_down,
        };
        if let Some(mut order) = slot.take() {
            order.mark_cancelled(ts_ms);
            self.order_history.push(order);
        }
    }

    pub fn avg_sum_if_buy_filled(&self, side: PositionSide, price: f64, qty: f64) -> f64 {
        if qty <= 0.0 {
            return self.position_avg_sum();
        }
        match side {
            PositionSide::Up => {
                weighted_avg(self.position_up.qty, self.position_up.avg_price, qty, price)
                    + self.position_down.avg_price
            }
            PositionSide::Down => {
                self.position_up.avg_price
                    + weighted_avg(
                        self.position_down.qty,
                        self.position_down.avg_price,
                        qty,
                        price,
                    )
            }
        }
    }

    pub fn mergeable_pairs(&self) -> f64 {
        self.position_up.qty.min(self.position_down.qty)
    }

    pub fn cash_pnl(&self) -> f64 {
        self.cash_received - self.cash_paid
    }

    pub fn inventory_value(&self, up_mid: f64, down_mid: f64) -> f64 {
        let pairs = self.mergeable_pairs();
        let extra_up = (self.position_up.qty - pairs).max(0.0);
        let extra_down = (self.position_down.qty - pairs).max(0.0);
        pairs * 1.00 + extra_up * up_mid + extra_down * down_mid
    }

    pub fn total_float_pnl(&self, up_mid: f64, down_mid: f64) -> f64 {
        self.position_up.float_pnl(up_mid) + self.position_down.float_pnl(down_mid)
    }

    pub fn net_pnl(&self, inventory_value: f64) -> f64 {
        self.cash_pnl() + inventory_value - self.total_fee + self.total_rebate
    }

    pub fn apply_merge(&mut self, pair_qty: f64, ts_ms: i64) {
        let pq = pair_qty.min(self.mergeable_pairs()).max(0.0);
        if pq <= 0.0 {
            return;
        }
        let avg_sum = self.position_avg_sum();
        self.merge_pnl += pq * (1.00 - avg_sum);
        self.cash_received += pq * 1.00;
        self.merged_pairs += pq;
        self.merge_count = self.merge_count.saturating_add(1);
        self.position_up.qty = (self.position_up.qty - pq).max(0.0);
        self.position_down.qty = (self.position_down.qty - pq).max(0.0);
        if self.position_up.qty <= 0.0 {
            self.position_up.avg_price = 0.0;
        }
        if self.position_down.qty <= 0.0 {
            self.position_down.avg_price = 0.0;
        }
        self.last_merge_ts_ms = ts_ms;
    }

    pub fn apply_fill(
        &mut self,
        side: PositionSide,
        buy_sell: bool,
        maker_taker: bool,
        price: f64,
        qty: f64,
        ts_ms: i64,
        window_end_ts: i64,
        up_bid: f64,
        up_ask: f64,
        down_bid: f64,
        down_ask: f64,
    ) {
        debug_assert!(buy_sell, "apply_fill 只支持 buy；sell 由 apply_merge 处理");
        if !buy_sell {
            return;
        }

        let p_clamped = price.clamp(0.0, 1.0);
        let taker_fee = qty * 0.072 * p_clamped * (1.0 - p_clamped);
        if maker_taker {
            self.total_rebate += taker_fee * 0.25;
        } else {
            self.total_fee += taker_fee;
        }
        self.cash_paid += price * qty;

        let pos = match side {
            PositionSide::Up => &mut self.position_up,
            PositionSide::Down => &mut self.position_down,
        };
        pos.avg_price = weighted_avg(pos.qty, pos.avg_price, qty, price);
        pos.qty += qty;

        self.trades_current_window.push(TradeRecord {
            side,
            buy_sell,
            maker_taker,
            price,
            qty,
            ts_ms,
            window_end_ts,
            up_ask,
            up_bid,
            up_spread: (up_ask - up_bid).max(0.0),
            down_ask,
            down_bid,
            down_spread: (down_ask - down_bid).max(0.0),
            snapshot_pnl: 0.0, // caller (AppState::apply_fill) stamps the real value after
        });
    }

    pub fn save_trades_for_window_and_clear(&mut self, window_end_ts: i64) -> io::Result<()> {
        let dir = Path::new("trades");
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("trades_{}.csv", window_end_ts));
        if !self.trades_current_window.is_empty() {
            let mut f = std::fs::File::create(&path)?;
            writeln!(f, "side,direction,maker_taker,price,qty,ts_ms,ts_iso,window_end_ts,up_ask,up_bid,up_spread,down_ask,down_bid,down_spread")?;
            for r in &self.trades_current_window {
                let side = match r.side {
                    PositionSide::Up => "UP",
                    PositionSide::Down => "DOWN",
                };
                let direction = if r.buy_sell { "buy" } else { "sell" };
                let mt = if r.maker_taker { "maker" } else { "taker" };
                let ts_iso = chrono::Utc
                    .timestamp_millis_opt(r.ts_ms)
                    .single()
                    .map(|t: chrono::DateTime<chrono::Utc>| {
                        t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
                    })
                    .unwrap_or_else(|| "".to_string());
                writeln!(
                    f,
                    "{},{},{},{:.6},{:.6},{},{},{},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6}",
                    side,
                    direction,
                    mt,
                    r.price,
                    r.qty,
                    r.ts_ms,
                    ts_iso,
                    r.window_end_ts,
                    r.up_ask,
                    r.up_bid,
                    r.up_spread,
                    r.down_ask,
                    r.down_bid,
                    r.down_spread
                )?;
            }
            f.flush()?;
        }
        self.trades_current_window.clear();
        Self::prune_trades_dir_keep_latest(dir, 10)?;
        Ok(())
    }

    /// 将本窗口 order_history 全量落盘到 orders/orders_<window_end>.csv 并清空。
    /// 每行 = 一笔 ManagedOrder 完整生命周期（含 worst_price / vwap / status / reject_reason），
    /// 供 scripts/window_report.py 分析 IOC 质量：fill_rate / worst_breach 率 / partial 率。
    pub fn save_orders_for_window_and_clear(&mut self, window_end_ts: i64) -> io::Result<()> {
        let dir = Path::new("orders");
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("orders_{}.csv", window_end_ts));
        let mut f = std::fs::File::create(&path)?;
        writeln!(
            f,
            "order_id,client_order_id,side,reason,target_price,worst_price,qty,filled_qty,vwap,fill_levels,status,reject_reason,placed_ts_ms,updated_ts_ms,placed_ts_iso,window_end_ts"
        )?;
        for o in &self.order_history {
            let reject = o.reject_reason.clone().unwrap_or_default();
            let vwap = o.vwap();
            let placed_iso = chrono::Utc
                .timestamp_millis_opt(o.placed_ts_ms)
                .single()
                .map(|t: chrono::DateTime<chrono::Utc>| {
                    t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
                })
                .unwrap_or_default();
            writeln!(
                f,
                "{},{},{},{},{:.4},{:.4},{:.4},{:.4},{:.6},{},{},{},{},{},{},{}",
                o.order_id,
                o.client_order_id,
                side_label(o.side),
                reason_label(o.reason),
                o.target_price,
                o.price,
                o.qty,
                o.filled_qty,
                vwap,
                o.fill_levels,
                status_label(o.status),
                reject,
                o.placed_ts_ms,
                o.updated_ts_ms,
                placed_iso,
                window_end_ts,
            )?;
        }
        f.flush()?;
        self.order_history.clear();
        Self::prune_orders_dir_keep_latest(dir, 10)?;
        Ok(())
    }

    pub fn reset_for_new_window(&mut self) {
        self.position_up = Position::default();
        self.position_down = Position::default();
        if let Some(mut order) = self.maker_buy_intent_up.take() {
            order.mark_expired(order.updated_ts_ms);
            self.order_history.push(order);
        }
        if let Some(mut order) = self.maker_buy_intent_down.take() {
            order.mark_expired(order.updated_ts_ms);
            self.order_history.push(order);
        }
        self.rebalance_hint_up = None;
        self.rebalance_hint_down = None;
    }

    pub fn settle_window_and_redeem(&mut self, strike_price: f64, binance_close: f64, ts_ms: i64) {
        let pairs = self.mergeable_pairs();
        if pairs > 0.0 {
            self.apply_merge(pairs, ts_ms);
        }
        if strike_price > 0.0 && binance_close > 0.0 {
            let up_wins = binance_close >= strike_price;
            if up_wins && self.position_up.qty > 0.0 {
                self.cash_received += self.position_up.qty * 1.0;
            } else if !up_wins && self.position_down.qty > 0.0 {
                self.cash_received += self.position_down.qty * 1.0;
            }
        }
    }

    fn prune_trades_dir_keep_latest(dir: &Path, keep: usize) -> io::Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path().extension().map_or(false, |ext| ext == "csv")
                    && e.file_name().to_string_lossy().starts_with("trades_")
            })
            .collect();
        entries.sort_by(|a, b| {
            let ts_a = a
                .path()
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.strip_prefix("trades_"))
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0);
            let ts_b = b
                .path()
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.strip_prefix("trades_"))
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0);
            ts_b.cmp(&ts_a)
        });
        for e in entries.into_iter().skip(keep) {
            let _ = std::fs::remove_file(e.path());
        }
        Ok(())
    }

    fn prune_orders_dir_keep_latest(dir: &Path, keep: usize) -> io::Result<()> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path().extension().map_or(false, |ext| ext == "csv")
                    && e.file_name().to_string_lossy().starts_with("orders_")
            })
            .collect();
        entries.sort_by(|a, b| {
            let ts_a = a
                .path()
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.strip_prefix("orders_"))
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0);
            let ts_b = b
                .path()
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.strip_prefix("orders_"))
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0);
            ts_b.cmp(&ts_a)
        });
        for e in entries.into_iter().skip(keep) {
            let _ = std::fs::remove_file(e.path());
        }
        Ok(())
    }
}

fn weighted_avg(old_qty: f64, old_avg: f64, add_qty: f64, add_price: f64) -> f64 {
    let total_qty = old_qty + add_qty;
    if total_qty > 0.0 {
        (old_avg * old_qty + add_price * add_qty) / total_qty
    } else {
        add_price
    }
}
