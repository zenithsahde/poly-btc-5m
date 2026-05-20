//! Wire format shared between the engine (server) and web-ui (WASM client).
//!
//! Mirror of TUI panels, projected from `engine::tui::app::AppState`.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DashboardSnapshot {
    pub header: HeaderInfo,
    pub orderbook: OrderbookView,
    pub trades: Vec<TradeRow>,
    pub latency: LatencyStats,
    pub poly: PolyView,
    pub position: PositionView,
    pub fair_value: FairValueView,
    pub delay: DelayStatsView,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HeaderInfo {
    pub symbol: String,
    pub ws_connected: bool,
    pub poly_ws_connected: bool,
    pub msg_rate: f64,
    pub total_msgs: u64,
    pub uptime: String,
    pub uptime_secs: u64,
    pub is_live_mode: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Level {
    pub price: f64,
    pub qty: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OrderbookView {
    pub best_bid: f64,
    pub best_ask: f64,
    pub spread_bps: f64,
    pub mid_price: f64,
    pub bids: Vec<Level>,
    pub asks: Vec<Level>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TradeRow {
    pub dir_time: String,
    pub is_buy: bool,
    pub price: f64,
    pub qty: f64,
    pub notional: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LatencyStats {
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub parse_p99_us: f64,
    pub msg_count: u64,
    pub last_warn_ms: Option<f64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PolyBook {
    pub bids: Vec<Level>,
    pub asks: Vec<Level>,
    pub best_bid: f64,
    pub best_ask: f64,
    pub last_trade_price: f64,
    pub last_trade_side: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PolyView {
    pub slug: String,
    pub token_id: String,
    pub window_end_ts: i64,
    pub expiry_minutes: f64,
    pub poly_delay_ms: Option<u64>,
    pub up: PolyBook,
    pub down: PolyBook,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PositionSide {
    pub qty: f64,
    pub avg_price: f64,
    pub float_pnl: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PositionView {
    pub up: PositionSide,
    pub down: PositionSide,
    pub avg_sum: f64,
    pub projected_avg_sum: Option<f64>,
    pub projected_qty_up: f64,
    pub projected_qty_down: f64,
    pub projected_skew: String,
    pub mergeable_pairs: f64,
    pub merged_pairs: f64,
    pub merge_count: u64,
    pub merge_pnl: f64,
    pub realized_pnl: f64,
    pub total_fee: f64,
    pub total_rebate: f64,
    pub cash_received: f64,
    pub cash_paid: f64,
    pub cash_pnl: f64,
    pub inventory_value: f64,
    pub total_float_pnl: f64,
    pub net_pnl: f64,
    pub chase_side: Option<String>,
    pub rebalance_hint_up: Option<(f64, f64)>,
    pub rebalance_hint_down: Option<(f64, f64)>,
    pub maker_buy_intent_up: Option<(f64, f64, i64)>,
    pub maker_buy_intent_down: Option<(f64, f64, i64)>,
    pub window_fill_rows: usize,
    pub window_up_qty: f64,
    pub window_up_vwap: f64,
    pub window_down_qty: f64,
    pub window_down_vwap: f64,
    pub window_notional: f64,
    pub ioc_orders: usize,
    pub ioc_fill_rate: f64,
    pub ioc_worst_breach_rate: f64,
    pub ioc_partial_rate: f64,
    pub ioc_avg_fill_levels: f64,
    pub ioc_max_fill_levels: u32,
    pub ioc_chase_orders: usize,
    pub ioc_chase_filled: usize,
    pub ioc_chase_fill_rate: f64,
    pub ioc_rebal_orders: usize,
    pub ioc_rebal_filled: usize,
    pub ioc_rebal_fill_rate: f64,
    pub ioc_last_order: Option<String>,
    pub last_action: Option<String>,
    pub pnl_history: Vec<PnlPointView>,
    pub order_events: Vec<OrderEventView>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PnlPointView {
    pub uptime_secs: u64,
    pub net_pnl: f64,
    pub cash_pnl: f64,
    pub inventory_value: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct OrderEventView {
    pub ts_ms: i64,
    pub kind: String,
    #[serde(default)]
    pub liquidity: String,
    pub side: String,
    pub summary: String,
    pub qty: f64,
    pub price: f64,
    pub pnl: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FairValueView {
    pub fair_price: f64,
    pub fair_price_down: f64,
    pub volatility_annual: f64,
    pub sticky_volatility: f64,
    pub sigma_used_default: bool,
    pub sigma_source: String,
    pub market_state: String,
    pub signal_gap_bps: f64,
    pub last_snipe_info: String,
    pub snipe_count: u64,
    pub snipe_threshold_bps: f64,
    pub iv_poly: f64,
    pub strike_price: f64,
    pub expiry_minutes: f64,
    pub poly_btc_offset: f64,
    pub poly_btc_price: f64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct DelayStatsView {
    pub count_pos: u32,
    pub sum_pos_ms: f64,
    pub min_pos_ms: f64,
    pub max_pos_ms: f64,
    pub count_neg: u32,
    pub sum_neg_ms: f64,
    pub min_neg_ms: f64,
    pub max_neg_ms: f64,
}
