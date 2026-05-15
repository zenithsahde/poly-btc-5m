//! Lightweight SQLite persistence for the Web UI history.

use std::path::Path;

use anyhow::Result;
use rusqlite::{params, Connection};

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
