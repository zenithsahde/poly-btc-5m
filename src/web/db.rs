//! 实盘订单 / 成交持久化（SQLite）。
//!
//! 三张表：
//!   * `pnl_samples`   —— Web UI 每 30s 一帧 PnL 时序快照
//!   * `my_orders`     —— POST /order 的完整回执（200/400 都写完整一行；resubmit 链通过
//!                        `parent_client_order_id` 关联）
//!   * `my_fills`      —— user-ws trade 事件 MATCHED 时入库；MINED / CONFIRMED 仅更新
//!                        status + tx_hash + last_update
//!
//! 写入路径单线程化：所有写操作通过 mpsc `DbMsg` 投递到 `run_db_writer`，POST 热路径不阻塞
//! sqlite IO。

use std::path::Path;

use anyhow::Result;
use rusqlite::{params, Connection};
use tokio::sync::mpsc;
use tracing::warn;

use crate::tui::app::WebPnlPoint;

const DB_PATH: &str = "webui_db/poly_btc_5m.sqlite3";

// ============================================================================
// Row 结构（POST/WS 事件入库时构造）
// ============================================================================

/// 一行 my_orders。POST 回包后构造，一次性 INSERT（成功/失败均写）。
#[derive(Debug, Clone)]
pub struct MyOrderRow {
    pub client_order_id: String,
    pub parent_client_order_id: Option<String>,
    pub attempt: u8,
    // 下单时
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
    pub reason: &'static str, // "chase" / "rebal"
    // 回执
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
    pub status: String, // 入库时通常 "MATCHED"
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

// ============================================================================
// 异步写入 channel
// ============================================================================

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
    let conn = open_and_init()?;
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let Err(e) = handle(&conn, msg) {
                warn!(error = %e, "db_writer write failed");
            }
        }
    });
    Ok(tx)
}

fn handle(conn: &Connection, msg: DbMsg) -> Result<()> {
    match msg {
        DbMsg::OrderRow(r) => insert_order_row(conn, &r),
        DbMsg::OrderCancelled { server_order_id, ts_ms } => {
            update_order_cancelled(conn, &server_order_id, ts_ms)
        }
        DbMsg::FillRow(r) => insert_fill_row(conn, &r),
        DbMsg::FillChainUpdate {
            trade_id,
            status,
            transaction_hash,
            last_update,
        } => update_fill_chain(
            conn,
            &trade_id,
            &status,
            transaction_hash.as_deref(),
            last_update,
        ),
        DbMsg::PnlSample(p) => insert_pnl_sample(conn, &p),
    }
}

// ============================================================================
// Connection 初始化
// ============================================================================

fn open_and_init() -> Result<Connection> {
    let path = Path::new(DB_PATH);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path)?;
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
    Ok(conn)
}

// ============================================================================
// 写入 helpers
// ============================================================================

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
