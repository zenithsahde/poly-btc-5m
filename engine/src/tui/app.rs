/// tui/app.rs - TUI 共享应用状态
/// 由策略引擎写入，由 UI 渲染器读取（Arc<RwLock<AppState>>）
use std::collections::VecDeque;
use std::io;
use std::time::Instant;

use crate::position::PositionLedger;
pub use crate::position::PositionSide as ChaseSide;

/// 面板展示用的单条成交记录
#[derive(Clone, Debug)]
pub struct TradeRow {
    /// 方向+时间，例如 "▲ 16:35:28"（主买）或 "▼ 16:35:28"（主卖）
    pub dir_time: String,
    /// true=主买，false=主卖（用于颜色）
    pub is_buy: bool,
    pub price: f64,
    pub qty: f64,
    pub notional: f64,
}

/// 订单簿档位
#[derive(Clone, Debug, Default)]
pub struct BookLevel {
    pub price: f64,
    pub qty: f64,
}

/// 延迟统计快照
#[derive(Clone, Debug, Default)]
pub struct LatencySnapshot {
    pub p50_ms: f64,
    pub p99_ms: f64,
    pub max_ms: f64,
    pub parse_p99_us: f64,
    pub msg_count: u64,
}

/// 激变态时的一次「领先」：我们进入激变态时刻的 FV 与 Poly 价，用于算 Poly 跟上延迟
#[derive(Clone, Debug)]
pub struct ExcitedLead {
    pub lead_start: Instant,
    pub fv_lead: f64,
    pub poly_mid_lead: f64,
}

/// 激变态延迟统计（单位：毫秒）；正延迟=FV 先变 Poly 后跟上，负延迟=Poly 先动我们后判定
#[derive(Clone, Debug, Default)]
pub struct DelayStats {
    pub count_pos: u32,
    pub sum_pos_ms: f64,
    pub min_pos_ms: f64,
    pub max_pos_ms: f64,
    pub count_neg: u32,
    pub sum_neg_ms: f64,
    pub min_neg_ms: f64,
    pub max_neg_ms: f64,
}

#[derive(Clone, Debug, Default)]
pub struct WebPnlPoint {
    pub uptime_secs: u64,
    pub net_pnl: f64,
    pub cash_pnl: f64,
    pub inventory_value: f64,
}

impl DelayStats {
    pub fn record_pos_ms(&mut self, delay_ms: f64) {
        self.count_pos = self.count_pos.saturating_add(1);
        self.sum_pos_ms += delay_ms;
        if self.count_pos == 1 {
            self.min_pos_ms = delay_ms;
            self.max_pos_ms = delay_ms;
        } else {
            self.min_pos_ms = self.min_pos_ms.min(delay_ms);
            self.max_pos_ms = self.max_pos_ms.max(delay_ms);
        }
    }
    pub fn record_neg_ms(&mut self, delay_ms: f64) {
        self.count_neg = self.count_neg.saturating_add(1);
        self.sum_neg_ms += delay_ms;
        if self.count_neg == 1 {
            self.min_neg_ms = delay_ms;
            self.max_neg_ms = delay_ms;
        } else {
            self.min_neg_ms = self.min_neg_ms.min(delay_ms);
            self.max_neg_ms = self.max_neg_ms.max(delay_ms);
        }
    }
}

/// 全局共享状态（线程安全，用 RwLock）
/// v0.4.9 加 Clone: TUI 渲染先 clone 副本立即释放读锁，避免与 signal.rs write 锁竞争
#[derive(Clone, Debug)]
pub struct AppState {
    /// 程序启动时间（用于计算运行时长）
    pub start_time: Instant,
    /// WebSocket 连接状态
    pub ws_connected: bool,
    /// 交易对
    pub symbol: String,
    /// 最优买价
    pub best_bid: f64,
    /// 最优卖价
    pub best_ask: f64,
    /// 买卖价差（bps）
    pub spread_bps: f64,
    /// 中间价
    pub mid_price: f64,
    /// 买单档位（价格降序，最多 10 档）
    pub bids: Vec<BookLevel>,
    /// 卖单档位（价格升序，最多 10 档）
    pub asks: Vec<BookLevel>,
    /// 最近成交（最多保留 20 条）
    pub recent_trades: VecDeque<TradeRow>,
    /// 延迟统计
    pub latency: LatencySnapshot,
    /// 消息速率（条/秒）
    pub msg_rate: f64,
    /// 上次统计时的消息数
    pub last_msg_count: u64,
    /// 上次统计时间
    pub last_rate_check: Instant,
    /// 总消息计数
    pub total_msgs: u64,
    /// 最近延迟告警（用于面板闪红）
    pub last_warn_ms: Option<f64>,
    /// Poly BTC 相对币安的固定价差（默认 -20.0 USDT）
    pub poly_btc_offset: f64,
    /// --- 延迟套利模型字段 ---
    /// 计算出的 BS 公允价格 UP (0.0 - 1.0)
    pub fair_price: f64,
    /// 计算出的 BS 公允价格 DOWN = 1 - UP
    pub fair_price_down: f64,
    /// 币安实时年化波动率 (例如 0.8)，已 clamp
    pub volatility_annual: f64,
    /// 当前 σ 是否来自默认值（样本不足时）
    pub sigma_used_default: bool,
    /// 粘性波动率：稳态时从 Poly 反解的 IV，激变态时沿用
    pub sticky_volatility: f64,
    /// 市场状态：稳态 / 激变态（决定是否信任 Poly 反解 IV）
    pub market_state: String,
    /// σ 来源：Poly IV / 粘性 / 默认
    pub sigma_source: String,
    /// Poly 隐含波动率 (校准用)
    pub iv_poly: f64,
    /// 市场行权价
    pub strike_price: f64,
    /// 距离到期分钟数（由 poly_window_end_ts 动态计算）
    pub expiry_minutes: f64,
    /// 公允价与盘口价的偏差 (bps)
    pub signal_gap_bps: f64,
    /// 模拟狙击日志 (例如: "SNIPE! Gap 18.2bps @ 0.5212")
    pub last_snipe_info: String,
    /// 触发次数
    pub snipe_count: u64,
    /// 触发阈值 (bps)
    pub snipe_threshold_bps: f64,
    /// 当前监控的 Polymarket Slug
    pub poly_market_slug: String,
    /// 当前 Poly Token ID（UP 侧；切换市场时更新，供下单使用）
    pub poly_token_id: String,
    /// 当前 Poly Token ID（DOWN 侧；切换市场时更新，实盘下单 DOWN 用）
    pub poly_down_token_id: String,
    /// 下一 5m 窗口切换时间戳 (Unix 秒)
    pub poly_window_end_ts: i64,
    /// Poly Up 订单簿买盘（价格降序，最多 15 档）
    pub poly_bids: Vec<BookLevel>,
    /// Poly Up 订单簿卖盘（价格升序，最多 15 档）
    pub poly_asks: Vec<BookLevel>,
    /// Poly Up 最优买/卖价、最后成交
    pub poly_best_bid: f64,
    pub poly_best_ask: f64,
    pub poly_last_trade_price: f64,
    pub poly_last_trade_side: String,
    /// Poly Down 完整订单簿与最优价、最后成交
    pub poly_down_bids: Vec<BookLevel>,
    pub poly_down_asks: Vec<BookLevel>,
    pub poly_down_best_bid: f64,
    pub poly_down_best_ask: f64,
    pub poly_down_last_trade_price: f64,
    pub poly_down_last_trade_side: String,
    /// Poly WS 是否已连接
    pub poly_ws_connected: bool,
    /// Poly 数据最后更新时间（用于 TUI 显示延迟）
    pub poly_last_update: Option<Instant>,
    /// 激变态领先：我们进入激变态时的 t_lead、FV_lead、poly_mid_lead
    pub excited_lead: Option<ExcitedLead>,
    /// 激变态延迟统计（正/负，毫秒）
    pub delay_stats: DelayStats,
    /// 上一帧 Poly mid（用于检测明显变动、负延迟）
    pub last_poly_mid: f64,
    /// 近期 Poly 明显变动 (发现变动的时刻, poly_mid)，用于负延迟匹配，保留约 5s
    pub recent_poly_moves: VecDeque<(Instant, f64)>,
    /// --- 做市/仓位与成交 ---
    pub ledger: PositionLedger,
    /// 当前可追涨侧（用于展示与浮亏减仓；UP 优先，若 UP 满足则显 UP）
    pub chase_side: Option<ChaseSide>,
    /// Web UI PnL 曲线采样。30s 一个点，保留 2 天，用于浏览器晚连接后仍能看到历史走势。
    pub web_pnl_history: VecDeque<WebPnlPoint>,
    pub last_web_pnl_sample_secs: Option<u64>,
}

impl AppState {
    pub fn new(symbol: &str) -> Self {
        Self {
            start_time: Instant::now(),
            ws_connected: false,
            symbol: symbol.to_string(),
            best_bid: 0.0,
            best_ask: 0.0,
            spread_bps: 0.0,
            mid_price: 0.0,
            bids: Vec::new(),
            asks: Vec::new(),
            recent_trades: VecDeque::with_capacity(20),
            latency: LatencySnapshot::default(),
            msg_rate: 0.0,
            last_msg_count: 0,
            last_rate_check: Instant::now(),
            total_msgs: 0,
            last_warn_ms: None,
            poly_btc_offset: -20.0,
            fair_price: 0.5,
            fair_price_down: 0.5,
            volatility_annual: 0.0,
            sigma_used_default: true,
            sticky_volatility: 0.0,
            market_state: "—".to_string(),
            sigma_source: "默认".to_string(),
            iv_poly: 0.0,
            strike_price: 96000.0,
            expiry_minutes: 5.0,
            signal_gap_bps: 0.0,
            last_snipe_info: "READY".to_string(),
            snipe_count: 0,
            snipe_threshold_bps: 12.0,
            poly_market_slug: "Finding...".to_string(),
            poly_token_id: String::new(),
            poly_down_token_id: String::new(),
            poly_window_end_ts: 0,
            poly_bids: Vec::new(),
            poly_asks: Vec::new(),
            poly_best_bid: 0.0,
            poly_best_ask: 0.0,
            poly_last_trade_price: 0.0,
            poly_last_trade_side: String::new(),
            poly_down_bids: Vec::new(),
            poly_down_asks: Vec::new(),
            poly_down_best_bid: 0.0,
            poly_down_best_ask: 0.0,
            poly_down_last_trade_price: 0.0,
            poly_down_last_trade_side: String::new(),
            poly_ws_connected: false,
            poly_last_update: None,
            excited_lead: None,
            delay_stats: DelayStats::default(),
            last_poly_mid: 0.0,
            recent_poly_moves: VecDeque::with_capacity(200), // ~5s at 40/s
            ledger: PositionLedger::default(),
            chase_side: None,
            web_pnl_history: VecDeque::with_capacity(5760),
            last_web_pnl_sample_secs: None,
        }
    }

    /// Poly 数据延迟（毫秒），无数据时返回 None
    pub fn poly_delay_ms(&self) -> Option<u64> {
        self.poly_last_update
            .map(|t| t.elapsed().as_millis() as u64)
    }

    /// 添加成交记录（最多保留 20 条）
    pub fn push_trade(&mut self, row: TradeRow) {
        if self.recent_trades.len() >= 20 {
            self.recent_trades.pop_back();
        }
        self.recent_trades.push_front(row);
    }

    pub fn record_web_pnl_sample(&mut self) -> Option<WebPnlPoint> {
        const SAMPLE_SECS: u64 = 30;
        const KEEP_POINTS: usize = 5760; // 2 days at 30s cadence

        let uptime_secs = self.start_time.elapsed().as_secs();
        if let Some(last) = self.last_web_pnl_sample_secs {
            if uptime_secs.saturating_sub(last) < SAMPLE_SECS {
                return None;
            }
        }

        let point = WebPnlPoint {
            uptime_secs,
            net_pnl: self.net_pnl(),
            cash_pnl: self.cash_pnl(),
            inventory_value: self.inventory_value(),
        };
        self.web_pnl_history.push_back(point.clone());
        self.last_web_pnl_sample_secs = Some(uptime_secs);

        while self.web_pnl_history.len() > KEEP_POINTS {
            self.web_pnl_history.pop_front();
        }
        Some(point)
    }

    /// 更新消息速率（每秒调用一次）
    pub fn update_rate(&mut self) {
        let elapsed = self.last_rate_check.elapsed().as_secs_f64();
        if elapsed >= 1.0 {
            let delta = self.total_msgs - self.last_msg_count;
            self.msg_rate = delta as f64 / elapsed;
            self.last_msg_count = self.total_msgs;
            self.last_rate_check = Instant::now();
        }
    }

    /// Poly UP 中价
    pub fn poly_up_mid(&self) -> f64 {
        if self.poly_best_bid > 0.0 && self.poly_best_ask > 0.0 {
            (self.poly_best_bid + self.poly_best_ask) / 2.0
        } else {
            0.0
        }
    }
    /// Poly DOWN 中价
    pub fn poly_down_mid(&self) -> f64 {
        if self.poly_down_best_bid > 0.0 && self.poly_down_best_ask > 0.0 {
            (self.poly_down_best_bid + self.poly_down_best_ask) / 2.0
        } else {
            0.0
        }
    }
    /// UP/DOWN 持仓均价之和（应 < 1）
    pub fn position_avg_sum(&self) -> f64 {
        self.ledger.position_avg_sum()
    }

    /// 若当前 Maker 买挂单均成交，UP 侧预估持仓量（用意图中的数量）
    pub fn projected_qty_up_after_intents(&self) -> f64 {
        self.ledger.projected_qty_up_after_intents()
    }

    /// 若当前 Maker 买挂单均成交，DOWN 侧预估持仓量
    pub fn projected_qty_down_after_intents(&self) -> f64 {
        self.ledger.projected_qty_down_after_intents()
    }

    /// 若当前 Maker 买挂单均成交，预估的仓位均价之和（用于配平与约束判断）
    pub fn projected_avg_sum_after_intents(&self) -> f64 {
        self.ledger.projected_avg_sum_after_intents()
    }

    /// 若在该侧再成交一笔买（price × qty），成交后的均价之和（用于配平条件：全配平价后须 < 1）
    pub fn avg_sum_if_buy_filled(&self, side: ChaseSide, price: f64, qty: f64) -> f64 {
        self.ledger.avg_sum_if_buy_filled(side, price, qty)
    }

    /// 总浮盈/浮亏（金额，未扣手续费）
    pub fn total_float_pnl(&self) -> f64 {
        self.ledger
            .total_float_pnl(self.poly_up_mid(), self.poly_down_mid())
    }
    /// (legacy 兼容名) 总盈亏 — v0.4.0-5m 改为 cash_pnl + inventory_value − cash_paid + rebate − fee
    /// 数学等价：(cash_received + inventory_value − cash_paid) − fee + rebate
    pub fn total_pnl_after_fee(&self) -> f64 {
        self.cash_pnl() + self.inventory_value() - self.ledger.total_fee
    }
    /// 净盈亏（v0.4.0-5m 重新框定）
    /// = cash_received(merge) + inventory_value(库存按市价) − cash_paid(buy) − taker_fee + maker_rebate
    pub fn net_pnl(&self) -> f64 {
        self.ledger.net_pnl(self.inventory_value())
    }
    /// 当前可虚拟 merge 的对数 = min(qty_up, qty_down)
    pub fn mergeable_pairs(&self) -> f64 {
        self.ledger.mergeable_pairs()
    }

    /// 现金账面 P&L = merge 收到的 USDC − buy 花出去的 USDC
    pub fn cash_pnl(&self) -> f64 {
        self.ledger.cash_pnl()
    }

    /// 库存按市价估值 = qty_up × poly_up_mid + qty_down × poly_down_mid
    /// 其中已可 merge 部分按 1.00 / pair 估值（= mergeable_pairs × 1.00）；超出部分按 mid
    pub fn inventory_value(&self) -> f64 {
        self.ledger
            .inventory_value(self.poly_up_mid(), self.poly_down_mid())
    }

    /// 应用一次虚拟 merge：扣减两侧 qty（avg 不变，因为按比例移除），增加 merge_pnl
    /// 经济意义：每对净利 = 1.00 − (avg_up + avg_down)
    pub fn apply_merge(&mut self, pair_qty: f64, ts_ms: i64) {
        self.ledger.apply_merge(pair_qty, ts_ms);
    }

    /// 应用一笔 maker buy 成交（v0.4.0-5m：只 buy，sell 全删）。
    ///   - 累加 cash_paid (USDC 流出)
    ///   - 更新仓位均价
    ///   - taker fee 公式 qty×0.072×p×(1−p)；maker 25% rebate
    ///   - sell 通过 merge 实现，不再走此路径
    pub fn apply_fill(
        &mut self,
        side: ChaseSide,
        buy_sell: bool,
        maker_taker: bool,
        price: f64,
        qty: f64,
        ts_ms: i64,
        up_bid: f64,
        up_ask: f64,
        down_bid: f64,
        down_ask: f64,
    ) {
        self.ledger.apply_fill(
            side,
            buy_sell,
            maker_taker,
            price,
            qty,
            ts_ms,
            self.poly_window_end_ts,
            up_bid,
            up_ask,
            down_bid,
            down_ask,
        );
    }

    /// 市场切换时：将本窗口成交记录写入文件（每笔含 side/方向/maker_taker/price/qty/ts_ms），再清空内存；只保留最新 10 份文件
    pub fn save_trades_for_window_and_clear(&mut self, window_end_ts: i64) -> io::Result<()> {
        self.ledger.save_trades_for_window_and_clear(window_end_ts)
    }

    /// 市场切换时：将本窗口订单生命周期记录写入文件，再清空内存；只保留最新 10 份文件
    pub fn save_orders_for_window_and_clear(&mut self, window_end_ts: i64) -> io::Result<()> {
        self.ledger.save_orders_for_window_and_clear(window_end_ts)
    }

    /// 市场切换后：清零本窗口仓位与挂单意图，新 5 分钟窗口从零库存开始
    /// v0.4.3-5m：所有 P&L 字段统一为跨窗口累积（fee / rebate / cash_* / merge_pnl 全保留）
    /// 仅清零物理上不能延续的字段（仓位/挂单/提示）—— 因为新窗口是新 token IDs，旧 qty 物理上失效
    pub fn reset_inventory_for_new_window(&mut self) {
        self.ledger.reset_for_new_window();
        self.chase_side = None;
        // v0.4.3-5m: total_fee / total_rebate / realized_pnl 不再 reset（跨窗口累积）
        // merged_pairs / merge_pnl / cash_received / cash_paid 仍跨窗口累积
    }

    /// v0.4.3-5m: 窗口结算前的 redeem 模拟
    ///   1. 先 force-merge 可配对部分（每对换 $1.00 USDC，避免输家边作废丢钱）
    ///   2. 再按 BTC 真实方向把赢家边残仓 redeem（每张 $1.00 USDC）
    ///   3. 输家边残仓直接清零（链上现实：作废）
    /// 调用时机：窗口切换前一刻（poly_client.rs:switch_to_next_market）
    pub fn settle_window_and_redeem(&mut self, binance_close: f64, ts_ms: i64) {
        self.ledger
            .settle_window_and_redeem(self.strike_price, binance_close, ts_ms);
    }

    /// Poly BTC 推算价格 = 币安 mid_price + poly_btc_offset
    pub fn poly_btc_price(&self) -> f64 {
        if self.mid_price > 0.0 {
            self.mid_price + self.poly_btc_offset
        } else {
            0.0
        }
    }

    /// 运行时长字符串
    pub fn uptime(&self) -> String {
        let secs = self.start_time.elapsed().as_secs();
        format!(
            "{:02}:{:02}:{:02}",
            secs / 3600,
            (secs % 3600) / 60,
            secs % 60
        )
    }
}
