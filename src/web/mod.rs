//! Web 持久化模块。
//!
//! 当前只暴露 SQLite 写入器（my_orders / my_fills / pnl_samples）。
//! 未来若要挂 axum HTTP / SSE 服务，可在本模块下另开 routes/snapshot/sse 子模块，
//! 不污染引擎主路径。

pub mod db;

// 公开再导出：当前 main.rs 直接走 `web::db::...`，外部消费者（未来的 web 服务）走这里。
#[allow(unused_imports)]
pub use db::{spawn_db_writer, DbMsg, DbSender, MyFillRow, MyOrderRow};
