//! SQLite 持久化。
//!
//! 历史路径（每 30s 由 `spawn_web_pnl_sampler` 调用 `persist_sample`）：
//!   * `pnl_samples`   —— Web UI PnL 曲线
//!   * `order_events`  —— 把 ledger 内 maker_buy_intent / trades_current_window /
//!                        order_history 整表落盘，供 Web UI 订单/成交面板查询
//!
//! 实盘事件驱动路径（POST /order 回包 + user-ws trade 事件）：
//!   * `my_orders`     —— POST /order 完整回执（200/400 都写整行；resubmit 链通过
//!                        parent_client_order_id 关联）
//!   * `my_fills`      —— user-ws trade 事件 MATCHED 入库；MINED/CONFIRMED 仅 UPDATE
//!                        status + tx_hash + last_update
//!   * 写路径单线程化：所有写操作经 mpsc `DbMsg` 投递到 `spawn_db_writer`，POST 热路径不阻塞 sqlite。

use std::path::Path;

use anyhow::Result;
use rusqlite::{params, Connection};
use tokio::sync::mpsc;
use tracing::warn;

use crate::position::{
    ManagedOrder, OrderStatus, PendingOrderReason, PositionSide as LedgerSide, TradeRecord,
};
use crate::tui::app::{AppState, WebPnlPoint};

const DB_PATH: &str = "webui_db/poly_btc_5m.sqlite3";

pub fn persist_sample(state: &AppState, point: &WebPnlPoint) -> Result<()> {
    let path = Path::new(DB_PATH);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut conn = Connection::open(path)?;
    init(&conn)?;

    let tx = conn.transaction()?;
    let now_ms = chrono::Utc::now().timestamp_millis();
    tx.execute(
        "INSERT OR IGNORE INTO pnl_samples
         (ts_ms, uptime_secs, net_pnl, cash_pnl, inventory_value)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            now_ms,
            point.uptime_secs as i64,
            point.net_pnl,
            point.cash_pnl,
            point.inventory_value
        ],
    )?;

    persist_pending_order(
        &tx,
        state.ledger.maker_buy_intent_up.as_ref(),
        state.poly_window_end_ts,
        point.net_pnl,
    )?;
    persist_pending_order(
        &tx,
        state.ledger.maker_buy_intent_down.as_ref(),
        state.poly_window_end_ts,
        point.net_pnl,
    )?;
    for trade in &state.ledger.trades_current_window {
        persist_trade(&tx, trade, point.net_pnl)?;
    }
    for order in &state.ledger.order_history {
        persist_order_terminal(&tx, order, state.poly_window_end_ts, point.net_pnl)?;
    }

    tx.commit()?;
    Ok(())
}

fn init(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA synchronous = NORMAL;

        CREATE TABLE IF NOT EXISTS pnl_samples (
            ts_ms INTEGER PRIMARY KEY,
            uptime_secs INTEGER NOT NULL,
            net_pnl REAL NOT NULL,
            cash_pnl REAL NOT NULL,
            inventory_value REAL NOT NULL
        );

        CREATE TABLE IF NOT EXISTS order_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            event_key TEXT NOT NULL UNIQUE,
            ts_ms INTEGER NOT NULL,
            window_end_ts INTEGER NOT NULL,
            order_id INTEGER,
            kind TEXT NOT NULL,
            side TEXT NOT NULL,
            reason TEXT,
            qty REAL NOT NULL,
            price REAL NOT NULL,
            filled_qty REAL NOT NULL,
            vwap REAL NOT NULL,
            status TEXT NOT NULL,
            pnl REAL NOT NULL,
            summary TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_order_events_ts ON order_events(ts_ms);
        CREATE INDEX IF NOT EXISTS idx_order_events_order ON order_events(order_id);

        CREATE TABLE IF NOT EXISTS my_orders (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            client_order_id TEXT UNIQUE NOT NULL,
            parent_client_order_id TEXT,
            attempt INTEGER NOT NULL DEFAULT 0,
            side TEXT NOT NULL,
            outcome TEXT NOT NULL,
            price REAL NOT NULL,
            size REAL NOT NULL,
            usd_value REAL NOT NULL,
            order_type TEXT NOT NULL,
            token_id TEXT NOT NULL,
            condition_id TEXT NOT NULL,
            ts_ms INTEGER NOT NULL,
            timestamp INTEGER,
            window_end_ts INTEGER NOT NULL,
            reason TEXT NOT NULL,
            success INTEGER NOT NULL,
            order_id TEXT,
            status TEXT,
            making_amount TEXT,
            taking_amount TEXT,
            transactions_hashes TEXT,
            trade_ids TEXT,
            error_msg TEXT,
            cancelled_ts_ms INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_my_orders_ts ON my_orders(ts_ms);
        CREATE INDEX IF NOT EXISTS idx_my_orders_order_id ON my_orders(order_id);

        CREATE TABLE IF NOT EXISTS my_fills (
            trade_id TEXT PRIMARY KEY,
            taker_order_id TEXT NOT NULL,
            market TEXT,
            asset_id TEXT NOT NULL,
            side TEXT NOT NULL,
            outcome TEXT NOT NULL,
            size REAL NOT NULL,
            price REAL NOT NULL,
            usd_value REAL NOT NULL,
            fee_rate_bps TEXT,
            status TEXT NOT NULL,
            matchtime INTEGER,
            last_update INTEGER,
            timestamp INTEGER NOT NULL,
            maker_orders TEXT,
            maker_address TEXT,
            transaction_hash TEXT,
            trader_side TEXT,
            window_end_ts INTEGER NOT NULL,
            ts_ms INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_my_fills_asset ON my_fills(asset_id);
        CREATE INDEX IF NOT EXISTS idx_my_fills_taker ON my_fills(taker_order_id);
        CREATE INDEX IF NOT EXISTS idx_my_fills_ts ON my_fills(ts_ms);
        ",
    )?;
    Ok(())
}

fn persist_pending_order(
    conn: &Connection,
    order: Option<&ManagedOrder>,
    window_end_ts: i64,
    pnl: f64,
) -> Result<()> {
    let Some(order) = order else {
        return Ok(());
    };
    insert_order_event(
        conn,
        &format!("post:{}:{}", order.order_id, order.placed_ts_ms),
        order.placed_ts_ms,
        window_end_ts,
        Some(order.order_id),
        "POST",
        side_name(order.side),
        Some(reason_name(order.reason)),
        order.remaining_qty(),
        order.price,
        order.filled_qty,
        order.vwap(),
        status_name(order.status),
        pnl,
        &format!(
            "POST {} {} {:.0}@{:.2}",
            side_name(order.side),
            reason_name(order.reason),
            order.remaining_qty(),
            order.price
        ),
    )
}

fn persist_trade(conn: &Connection, trade: &TradeRecord, pnl: f64) -> Result<()> {
    let kind = if trade.maker_taker {
        "FILL_MAKER"
    } else {
        "FILL_TAKER"
    };
    insert_order_event(
        conn,
        &format!(
            "trade:{}:{}:{:.6}:{:.6}:{}",
            trade.ts_ms,
            side_name(trade.side),
            trade.price,
            trade.qty,
            kind
        ),
        trade.ts_ms,
        trade.window_end_ts,
        None,
        kind,
        side_name(trade.side),
        None,
        trade.qty,
        trade.price,
        trade.qty,
        trade.price,
        "filled",
        pnl,
        &format!(
            "{} {} {:.0}@{:.2}",
            kind,
            side_name(trade.side),
            trade.qty,
            trade.price
        ),
    )
}

fn persist_order_terminal(
    conn: &Connection,
    order: &ManagedOrder,
    window_end_ts: i64,
    pnl: f64,
) -> Result<()> {
    insert_order_event(
        conn,
        &format!(
            "order:{}:{}:{}",
            order.order_id,
            order.updated_ts_ms,
            status_name(order.status)
        ),
        order.updated_ts_ms,
        window_end_ts,
        Some(order.order_id),
        order_event_kind(order),
        side_name(order.side),
        Some(reason_name(order.reason)),
        order.qty,
        order.price,
        order.filled_qty,
        order.vwap(),
        status_name(order.status),
        pnl,
        &format!(
            "{} {} {} {:.0}@{:.2}",
            order_event_kind(order),
            side_name(order.side),
            reason_name(order.reason),
            order.qty,
            order.price
        ),
    )
}

#[allow(clippy::too_many_arguments)]
fn insert_order_event(
    conn: &Connection,
    event_key: &str,
    ts_ms: i64,
    window_end_ts: i64,
    order_id: Option<u64>,
    kind: &str,
    side: &str,
    reason: Option<&str>,
    qty: f64,
    price: f64,
    filled_qty: f64,
    vwap: f64,
    status: &str,
    pnl: f64,
    summary: &str,
) -> Result<()> {
    tx_execute(
        conn,
        event_key,
        ts_ms,
        window_end_ts,
        order_id.map(|v| v as i64),
        kind,
        side,
        reason,
        qty,
        price,
        filled_qty,
        vwap,
        status,
        pnl,
        summary,
    )
}

#[allow(clippy::too_many_arguments)]
fn tx_execute(
    conn: &Connection,
    event_key: &str,
    ts_ms: i64,
    window_end_ts: i64,
    order_id: Option<i64>,
    kind: &str,
    side: &str,
    reason: Option<&str>,
    qty: f64,
    price: f64,
    filled_qty: f64,
    vwap: f64,
    status: &str,
    pnl: f64,
    summary: &str,
) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO order_events
         (event_key, ts_ms, window_end_ts, order_id, kind, side, reason, qty, price,
          filled_qty, vwap, status, pnl, summary)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            event_key,
            ts_ms,
            window_end_ts,
            order_id,
            kind,
            side,
            reason,
            qty,
            price,
            filled_qty,
            vwap,
            status,
            pnl,
            summary
        ],
    )?;
    Ok(())
}

fn order_event_kind(order: &ManagedOrder) -> &'static str {
    if order.filled_qty > 0.0 {
        "MATCH"
    } else if order.reject_reason.as_deref() == Some("worst_breach") {
        "REJECT"
    } else {
        match order.status {
            OrderStatus::Cancelled => "CANCEL",
            OrderStatus::Expired => "CANCEL",
            OrderStatus::Rejected => "REJECT",
            _ => "ORDER",
        }
    }
}

fn side_name(side: LedgerSide) -> &'static str {
    match side {
        LedgerSide::Up => "UP",
        LedgerSide::Down => "DOWN",
    }
}

fn reason_name(reason: PendingOrderReason) -> &'static str {
    match reason {
        PendingOrderReason::Chase => "chase",
        PendingOrderReason::Rebalance => "rebal",
        PendingOrderReason::JumpChase => "jump_chase",
    }
}

fn status_name(status: OrderStatus) -> &'static str {
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

// ============================================================================
// 实盘事件驱动路径：DbMsg → spawn_db_writer
// ============================================================================

/// 一行 my_orders。POST 回包后构造，一次性 INSERT（成功/失败均写）。
#[derive(Debug, Clone)]
pub struct MyOrderRow {
    pub client_order_id: String,
    pub parent_client_order_id: Option<String>,
    pub attempt: u8,
    pub side: &'static str,    // "buy" / "sell"
    pub outcome: &'static str, // "up" / "down"
    pub price: f64,
    pub size: f64,
    pub usd_value: f64,
    pub order_type: &'static str, // "FAK" / "GTC"
    pub token_id: String,
    pub condition_id: String,
    pub ts_ms: i64,
    pub timestamp: Option<i64>,
    pub window_end_ts: i64,
    pub reason: &'static str, // "chase" / "rebal" / "jump_chase"
    pub success: bool,
    pub order_id: Option<String>,
    pub status: Option<String>,
    pub making_amount: Option<String>,
    pub taking_amount: Option<String>,
    pub transactions_hashes_json: Option<String>,
    pub trade_ids_json: Option<String>,
    pub error_msg: Option<String>,
}

/// 一行 my_fills。user-ws trade 事件 MATCHED 时构造。
#[derive(Debug, Clone)]
pub struct MyFillRow {
    pub trade_id: String,
    pub taker_order_id: String,
    pub market: Option<String>,
    pub asset_id: String,
    pub side: &'static str,
    pub outcome: &'static str,
    pub size: f64,
    pub price: f64,
    pub usd_value: f64,
    pub fee_rate_bps: Option<String>,
    pub status: String,
    pub matchtime: Option<i64>,
    pub last_update: Option<i64>,
    pub timestamp: i64,
    pub maker_orders_json: Option<String>,
    pub maker_address: Option<String>,
    pub transaction_hash: Option<String>,
    pub trader_side: Option<String>,
    pub window_end_ts: i64,
    pub ts_ms: i64,
}

#[derive(Debug)]
pub enum DbMsg {
    OrderRow(MyOrderRow),
    OrderCancelled {
        server_order_id: String,
        ts_ms: i64,
    },
    FillRow(MyFillRow),
    FillChainUpdate {
        trade_id: String,
        status: String,
        transaction_hash: Option<String>,
        last_update: Option<i64>,
    },
    PnlSample(WebPnlPoint),
}

pub type DbSender = mpsc::UnboundedSender<DbMsg>;

/// 启动 long-lived sqlite writer 任务。
pub fn spawn_db_writer() -> Result<DbSender> {
    let (tx, mut rx) = mpsc::unbounded_channel::<DbMsg>();
    let path = Path::new(DB_PATH);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path)?;
    init(&conn)?;
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let Err(e) = handle_db_msg(&conn, msg) {
                warn!(error = %e, "db_writer write failed");
            }
        }
    });
    Ok(tx)
}

fn handle_db_msg(conn: &Connection, msg: DbMsg) -> Result<()> {
    match msg {
        DbMsg::OrderRow(r) => insert_order_row(conn, &r),
        DbMsg::OrderCancelled {
            server_order_id,
            ts_ms,
        } => update_order_cancelled(conn, &server_order_id, ts_ms),
        DbMsg::FillRow(r) => insert_fill_row(conn, &r),
        DbMsg::FillChainUpdate {
            trade_id,
            status,
            transaction_hash,
            last_update,
        } => update_fill_chain(conn, &trade_id, &status, transaction_hash.as_deref(), last_update),
        DbMsg::PnlSample(p) => insert_pnl_sample(conn, &p),
    }
}

fn insert_order_row(conn: &Connection, r: &MyOrderRow) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO my_orders
         (client_order_id, parent_client_order_id, attempt,
          side, outcome, price, size, usd_value, order_type, token_id, condition_id,
          ts_ms, timestamp, window_end_ts, reason,
          success, order_id, status, making_amount, taking_amount,
          transactions_hashes, trade_ids, error_msg)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                 ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)",
        params![
            r.client_order_id,
            r.parent_client_order_id,
            r.attempt as i64,
            r.side,
            r.outcome,
            r.price,
            r.size,
            r.usd_value,
            r.order_type,
            r.token_id,
            r.condition_id,
            r.ts_ms,
            r.timestamp,
            r.window_end_ts,
            r.reason,
            r.success as i64,
            r.order_id,
            r.status,
            r.making_amount,
            r.taking_amount,
            r.transactions_hashes_json,
            r.trade_ids_json,
            r.error_msg,
        ],
    )?;
    Ok(())
}

fn update_order_cancelled(conn: &Connection, server_order_id: &str, ts_ms: i64) -> Result<()> {
    conn.execute(
        "UPDATE my_orders SET status = 'cancelled', cancelled_ts_ms = ?1
         WHERE order_id = ?2",
        params![ts_ms, server_order_id],
    )?;
    Ok(())
}

fn insert_fill_row(conn: &Connection, r: &MyFillRow) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO my_fills
         (trade_id, taker_order_id, market, asset_id, side, outcome,
          size, price, usd_value, fee_rate_bps, status,
          matchtime, last_update, timestamp,
          maker_orders, maker_address, transaction_hash, trader_side,
          window_end_ts, ts_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                 ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
        params![
            r.trade_id,
            r.taker_order_id,
            r.market,
            r.asset_id,
            r.side,
            r.outcome,
            r.size,
            r.price,
            r.usd_value,
            r.fee_rate_bps,
            r.status,
            r.matchtime,
            r.last_update,
            r.timestamp,
            r.maker_orders_json,
            r.maker_address,
            r.transaction_hash,
            r.trader_side,
            r.window_end_ts,
            r.ts_ms,
        ],
    )?;
    Ok(())
}

fn update_fill_chain(
    conn: &Connection,
    trade_id: &str,
    status: &str,
    transaction_hash: Option<&str>,
    last_update: Option<i64>,
) -> Result<()> {
    conn.execute(
        "UPDATE my_fills
         SET status = ?1,
             transaction_hash = COALESCE(?2, transaction_hash),
             last_update = COALESCE(?3, last_update)
         WHERE trade_id = ?4",
        params![status, transaction_hash, last_update, trade_id],
    )?;
    Ok(())
}

fn insert_pnl_sample(conn: &Connection, p: &WebPnlPoint) -> Result<()> {
    let now_ms = chrono::Utc::now().timestamp_millis();
    conn.execute(
        "INSERT OR IGNORE INTO pnl_samples
         (ts_ms, uptime_secs, net_pnl, cash_pnl, inventory_value)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            now_ms,
            p.uptime_secs as i64,
            p.net_pnl,
            p.cash_pnl,
            p.inventory_value
        ],
    )?;
    Ok(())
}
