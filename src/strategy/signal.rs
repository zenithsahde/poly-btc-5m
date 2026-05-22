/// strategy/signal.rs - 策略引擎
/// 消费 MarketEvent，将数据写入 AppState 供 TUI 展示（无下单/撤单逻辑）
/// 实现文档：Poly IV + 稳态/激变态 + 粘性波动率
use std::collections::VecDeque;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use chrono::Local;
use tokio::sync::broadcast;
use tracing::warn;

use crate::{
    config::AppConfig,
    execution::client::{BuyIntent, OrderClient},
    model::{orderbook::LocalOrderBook, ticker::BestBidAsk, trade::Trade},
    position::PendingOrderReason,
    strategy::{
        bs_model,
        excited_snapshots::{ExcitedSnapshotWriter, SnapshotRow},
        fv_snapshots::{FvRow, FvSnapshotWriter},
        volatility::RollingVolatility,
    },
    tui::app::{AppState, BookLevel, ChaseSide, ExcitedLead, TradeRow},
    ws::stream::MarketEvent,
};

// 5m 阈值重拟（vs 15m baseline）：
//   - 总时长 1/3 → buffer/timeout 同比例缩
//   - 5m 内 BTC 期望平均波动约为 15m 的 1/√3 ≈ 58%（√t 缩放），5s/1s 阈值按此略缩
//   - 价差/价格类阈值（POLY_SPREAD_NARROW_MAX / CATCHUP_EPSILON / CHASE_GAP_MIN / TICK）不随期限变化
/// 稳态判定：币安 5 秒波动 < 此值 (USD)。15m=$5 → 5m=$3（按 √t 缩放）
const STEADY_MOVE_5S_MAX: f64 = 3.0;
/// 激变态判定：币安 1 秒波动 > 此值 (USD) 即认为 Poly 滞后，停用当前盘口反解 IV。15m=$50 → 5m=$30
const EXCITED_MOVE_1S_MIN: f64 = 30.0;
/// Poly 价差「窄」：买卖差 < 此值 (0.02 = 2 美分)
const POLY_SPREAD_NARROW_MAX: f64 = 0.02;
/// Poly 跟上 FV 的阈值（正负 3 分）
const CATCHUP_EPSILON: f64 = 0.03;
/// Poly 明显变动阈值（用于记录 recent_poly_moves 与负延迟）
const POLY_MOVE_THRESH: f64 = 0.03;
/// 领先超时：超过此时长未跟上则丢弃本次 lead。15m=30s → 5m=10s（10s 已是 5m 的 1/30，足够）
const LEAD_TIMEOUT_MS: u64 = 10_000;
/// 追涨门槛：FV 比 Poly 高 ≥ 10 美分（v0.4.8 从 7c 提到 10c）
/// 回测 472K 行实证：gap=0.07 fee=$102 PnL=+$308；gap=0.10 fee=$52 PnL=+$329。fee 减半，PnL +7%
/// 底层逻辑：force-balance 在 BTC 单向走时强制配对追贵对侧，gap 越严越能过滤"假 lead"
const CHASE_GAP_MIN: f64 = 0.10;
/// FV 短期历史长度（用于判定「上涨」）
const FV_HISTORY_LEN: usize = 10;
/// Poly 最小报价单位（1 美分）
const TICK: f64 = 0.01;
/// 追涨侧 Maker 买挂单数量（张）—— Polymarket 强制 5 张最低，此处用 100 张作资金体量
const MAKER_BUY_CHASE_MIN_QTY: f64 = 100.0;
/// Merge 触发：可配对张数 ≥ 此值时进行虚拟 merge（凑够 5 对再 merge，省 gas 但模拟里无此约束）
const MERGE_TRIGGER_PAIR_QTY: f64 = 1.0;
/// Merge 节流：两次 merge 至少间隔此毫秒数
const MERGE_INTERVAL_MS: i64 = 1000;

/// 追涨仅在激变态下进行：稳态时不挂追涨单、不保留追涨侧，避免在无波动时误建仓
const CHASE_ONLY_IN_EXCITED: bool = true;

/// (v0.4.5 deprecated: taker-only 模式不需要 fill 概率，taker 100% 成交)
#[allow(dead_code)]
const MAKER_FILL_PROBABILITY: f64 = 0.5;
/// (v0.4.5 deprecated: taker-only 模式不需要撤单延迟)
#[allow(dead_code)]
const CANCEL_DELAY_MS: i64 = 200;
/// v0.4.5 taker-only：同侧 taker buy 最小间隔（防止每帧 BookTicker 触发暴买）
const TAKER_BUY_INTERVAL_MS: i64 = 1000;

// ── P0 三守门（v0.4.1-5m 引入，防止 100 张档尾部风险灾难）──
/// 单边追涨/配平价格上限。> 0.85 价位的"几乎确定"赌局风险:收益不对称（1c edge vs 49:1 损失），
/// 一笔可吞 40 个健康窗口的累积。挂价超此值时拒绝建仓。
const MAX_BUY_PRICE: f64 = 0.85;
/// v0.4.6 force-balance（E0）：chase 时只允许买"持仓 ≤ 对侧" 的方向。
/// 回测实证：E0 把 PnL 从 -$1501 提升到 +$110，最大单窗口回撤从 -$1216 降到 -$43。
/// 底层逻辑：lead alpha 是单边瞬时优势，但反复追同一边会累积 avg_sum > 1 的"半死对"。
/// 强制配平 = 每笔 chase 必然配对，把 lead alpha 100% 转化为 merge 套利空间。
const FORCE_BALANCE_SLACK: f64 = 0.0;
/// v0.4.10 健康对守门（C2 cost-aware）：建仓腿（对侧空仓）fill 后 avg_sum < 此值
/// 用对侧 ask 作 proxy。1.05 容许 Polymarket spread 1c + 4c lead 兑现 buffer。
const PAIR_HEALTH_MAX: f64 = 1.05;
/// v0.4.14 配平腿严格守门：对侧已有持仓时 fill 后 avg_sum < 0.98
/// 用真实 avg 计算（不用 ask proxy）。0.98 = 留 2c 给 fee + 滑点，余下 ≥ 2c 锁定净利。
/// 防 v0.4.13 看到的"avg_sum 漂到 1.01 卡死 merge"动态滞后 bug。
const REBAL_HEALTH_MAX: f64 = 0.98;
/// v0.4.11 Marketable Limit Order: 建仓腿目标价 = FV - SAFETY_MARGIN（吃 lead alpha）
/// 回测 1.2M 行实证：margin=0.05 时 PnL +$978（vs v0.4.10 +$222，4x 提升）
const SAFETY_MARGIN: f64 = 0.05;
/// v0.4.13 配平腿专用 margin：target = (1 - opp.avg) - REBALANCE_MARGIN
/// 配平腿不抓 lead alpha，只锁套利空间。0.02 = 留 2c 给 fee + 滑点，余下确定净利。
const REBALANCE_MARGIN: f64 = 0.02;
/// Maker 挂单超时（毫秒）：挂单超过此时间未 fill → cancel
/// v0.4.15: 3s → 5s（实测最近 50min 6/11 窗口因 timeout 失配 → 1 笔 stuck 单边。延长拯救 50% 单边）
const MAKER_TIMEOUT_MS: i64 = 5000;
/// FV 移动 ≥ 此 tick 数时 cancel + 重挂新价（防 BTC 反向时被 adverse fill）
const REPRICE_TICK_THRESH: f64 = 0.02; // 2 tick
/// 距窗口结束 < 此分钟数时停止建仓（chase 路径 + 偏仓首次进入路径都禁）
/// v0.4.15: 0.5 → 1.5（实证：末段建仓导致"1 笔 stuck 单边"窗口占 50%）
/// 1.5min 给配对腿足够时间 fill；剩余 < 1.5min 不再建新仓
const MIN_EXPIRY_MIN_FOR_CHASE: f64 = 1.5;
/// 距窗口结束 < 此分钟数时配平腿仍允许（拯救已建仓的偏仓）
/// 0.5min 是 force_merge 的下限，> 0.5min 仍允许配平挂单 fill
const MIN_EXPIRY_MIN_FOR_REBAL: f64 = 0.5;
/// Merge 守门：avg_sum > 1.0 + EPS 时拒绝 merge（避免主动锁亏）。
/// 等价于"配错方向后等待 redeem 而非主动 merge 锁定亏损"。
const MERGE_AVG_SUM_MAX: f64 = 1.0;
/// P1.2：窗口末段强制 merge 阈值。expiry_min < 此值时绕过 MERGE_AVG_SUM_MAX 守门，
/// 全量 merge 已配对部分。底层逻辑：每对 merge=$1.00，与 redeem 数学等价；
/// 但 merge 即时锁定，避免残仓过夜的链上 gas + 时机风险。
const FORCE_MERGE_EXPIRY_MIN: f64 = 1.0;

/// IV EMA 平滑系数 α。每帧反解出的 IV 加权 α，旧值加权 (1-α)。
/// α=0.02 ≈ 时间常数 50 帧 ≈ 5s @ 100ms/tick。让 σ 反映市场共识但不被 Binance 瞬时跳动吸收。
const SIGMA_EMA_ALPHA: f64 = 0.02;

pub struct SignalEngine {
    config: AppConfig,
    orderbook: LocalOrderBook,
    state: Arc<RwLock<AppState>>,
    order_client: Arc<dyn OrderClient>,
    vol_calc: RollingVolatility,
    /// 币安 mid 历史 (ts_ms, mid)，用于稳态/激变态判定，保留约 5 秒
    binance_mid_history: VecDeque<(u64, f64)>,
    /// 上一帧是否为稳态（用于检测 稳态→激变态 开启 lead）
    last_was_steady: bool,
    /// 激变态快照：按窗口写 CSV，保留最近 10 个 5 分钟市场
    excited_snapshot_writer: ExcitedSnapshotWriter,
    /// FV_up 短期历史（判定上涨）
    fv_up_history: VecDeque<f64>,
    /// FV_down 短期历史（判定上涨）
    fv_down_history: VecDeque<f64>,
    /// EMA 平滑后的 IV（来自 Poly 反解），用于 BS 定价。0 表示尚未初始化。
    sigma_ema: f64,
    /// 全量快照采集器（v0.3.3）：每个 BookTicker / PolyBookUpdate 都落盘
    fv_snapshot_writer: FvSnapshotWriter,
    /// v0.4.5: 同侧 taker buy 节流时间戳
    last_taker_up_ts_ms: i64,
    last_taker_down_ts_ms: i64,
}

impl SignalEngine {
    pub fn new(
        config: AppConfig,
        state: Arc<RwLock<AppState>>,
        order_client: Arc<dyn OrderClient>,
    ) -> Self {
        let depth_limit = config.orderbook.depth_levels;
        let symbol = config.trading.symbol.clone();
        Self {
            config,
            orderbook: LocalOrderBook::new(&symbol, depth_limit),
            state,
            order_client,
            vol_calc: RollingVolatility::new(500),
            binance_mid_history: VecDeque::with_capacity(200),
            last_was_steady: false,
            excited_snapshot_writer: ExcitedSnapshotWriter::new(),
            fv_up_history: VecDeque::with_capacity(FV_HISTORY_LEN + 5),
            fv_down_history: VecDeque::with_capacity(FV_HISTORY_LEN + 5),
            sigma_ema: 0.0,
            fv_snapshot_writer: FvSnapshotWriter::new(),
            last_taker_up_ts_ms: 0,
            last_taker_down_ts_ms: 0,
        }
    }

    /// FV 是否在上涨：当前值 > 近期均值
    fn fv_rising(history: &VecDeque<f64>, current: f64) -> bool {
        if history.len() < 3 {
            return false;
        }
        let mean: f64 = history.iter().sum::<f64>() / history.len() as f64;
        current > mean
    }

    /// 过去 5 秒、1 秒内币安 mid 的波动 (max - min)，用于稳态/激变态判定
    fn move_range(&self, now_ts_ms: u64) -> (f64, f64) {
        let cutoff_1s = now_ts_ms.saturating_sub(1000);
        let cutoff_5s = now_ts_ms.saturating_sub(5000);
        let (mut min_1, mut max_1, mut min_5, mut max_5) = (f64::MAX, f64::MIN, f64::MAX, f64::MIN);
        let mut has_1 = false;
        let mut has_5 = false;
        for &(t, m) in &self.binance_mid_history {
            if t >= cutoff_5s {
                has_5 = true;
                min_5 = min_5.min(m);
                max_5 = max_5.max(m);
            }
            if t >= cutoff_1s {
                has_1 = true;
                min_1 = min_1.min(m);
                max_1 = max_1.max(m);
            }
        }
        let move_5s = if has_5 { (max_5 - min_5).max(0.0) } else { 0.0 };
        let move_1s = if has_1 { (max_1 - min_1).max(0.0) } else { 0.0 };
        (move_5s, move_1s)
    }

    pub async fn run(mut self, mut event_rx: broadcast::Receiver<MarketEvent>) {
        loop {
            match event_rx.recv().await {
                Ok(event) => {
                    self.handle_event(event);
                    // 更新消息速率
                    if let Ok(mut s) = self.state.write() {
                        s.total_msgs += 1;
                        s.ws_connected = true;
                        s.update_rate();
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("策略引擎落后 {} 条消息", n);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    break;
                }
            }
        }
    }

    fn handle_event(&mut self, event: MarketEvent) {
        match event {
            // ── Book Ticker ──────────────────────────────────────────
            MarketEvent::BookTicker { data, .. } => {
                if let Ok(bba) = BestBidAsk::try_from(&data) {
                    let spread = bba.spread_bps();
                    let mid = bba.mid_price();
                    let now_ts_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;

                    // 更新币安 mid 历史，用于稳态/激变态判定
                    self.binance_mid_history.push_back((now_ts_ms, mid));
                    let cutoff_5s = now_ts_ms.saturating_sub(5000);
                    while self.binance_mid_history.front().map(|&(t, _)| t < cutoff_5s).unwrap_or(false) {
                        self.binance_mid_history.pop_front();
                    }

                    // 闭包外预读 sigma_ema（self 字段），让 read-lock 闭包能用、闭包外能写
                    let sigma_ema_prev = self.sigma_ema;
                    let (strike, expiry_min, sigma, sigma_used_default, sigma_source, market_state, sticky_vol, current_poly_p, steady, iv_raw_for_ema) =
                        if let Ok(s) = self.state.read() {
                            let s = &*s;
                            let now_ts = chrono::Utc::now().timestamp();
                            let expiry_min = if s.poly_window_end_ts <= 0 {
                                5.0
                            } else {
                                ((s.poly_window_end_ts - now_ts) as f64 / 60.0).max(0.0)
                            };
                            let strike = if s.strike_price > 0.0 { s.strike_price } else { mid.round() };
                            let up_valid = s.poly_best_bid > 0.0 && s.poly_best_ask > 0.0;
                            let down_valid = s.poly_down_best_bid > 0.0 && s.poly_down_best_ask > 0.0;
                            // Up 中价（用于反解 IV：BS 给出的是 Up 概率）
                            let up_mid = up_valid.then(|| {
                                let (lo, hi) = (
                                    s.poly_best_bid.min(s.poly_best_ask),
                                    s.poly_best_bid.max(s.poly_best_ask),
                                );
                                (lo + hi) / 2.0
                            });
                            // Down 中价；Up + Down ≈ 1，无 Up 时用 1 - Down 作为隐含 Up 价
                            let down_mid = down_valid.then(|| {
                                let (lo, hi) = (
                                    s.poly_down_best_bid.min(s.poly_down_best_ask),
                                    s.poly_down_best_bid.max(s.poly_down_best_ask),
                                );
                                (lo + hi) / 2.0
                            });
                            let current_poly_p = up_mid
                                .unwrap_or_else(|| down_mid.map(|d| (1.0 - d).clamp(0.0, 1.0)).unwrap_or(0.5));
                            let t_years = bs_model::minutes_to_years(expiry_min);

                            let (move_5s, move_1s) = self.move_range(now_ts_ms);
                            let (poly_spread, has_poly) = if up_valid {
                                ((s.poly_best_ask - s.poly_best_bid).max(0.0), true)
                            } else if down_valid {
                                ((s.poly_down_best_ask - s.poly_down_best_bid).max(0.0), true)
                            } else {
                                (1.0, false)
                            };
                            let poly_spread_narrow = poly_spread < POLY_SPREAD_NARROW_MAX;

                            // 稳态：币安 5s 波动小、1s 未暴动、Poly 价差窄且有效
                            let steady = move_5s < STEADY_MOVE_5S_MAX
                                && move_1s < EXCITED_MOVE_1S_MIN
                                && has_poly
                                && poly_spread_narrow;

                            // 前瞻 FV (v0.3.2-5m)：σ 来自「Poly IV 反解 + EMA 平滑」，FV = N(d2(S_now, K, T, σ_ema))
                            //   - 反解保留：σ 数值与 Poly 共识对齐（市场对未来波动的预期）
                            //   - EMA 平滑（α=0.02，τ≈5s）：吸收掉 Binance 瞬时跳动对 IV 的虚假压低
                            //   - 当 Binance 跳但 Poly 没跟：iv_raw 瞬时下跌，σ_ema 几乎不动 → d2 因 S 上升而上升 → FV 正确追涨
                            //   详见 docs/forward-looking-fv.md
                            let smin = self.config.trading.volatility_sigma_min;
                            let smax_poly = self.config.trading.volatility_sigma_max_poly;
                            let iv_raw = if has_poly {
                                bs_model::find_implied_volatility(mid, strike, t_years, current_poly_p)
                            } else {
                                0.0
                            };
                            // 稳态优先用 Poly IV 喂 EMA；激变态时反解 IV 不可信，跳过本帧 EMA 更新
                            let _sigma_ema_alpha = SIGMA_EMA_ALPHA; // 留作后续配置化的 hook
                            // σ 选择优先级：
                            //   1. 已初始化的 σ_ema（最稳）：用 EMA 平滑值，吸收 Binance 瞬时 noise
                            //   2. 首次启动 + 稳态 + iv_raw 有效：直接用 iv_raw 初始化
                            //   3. 粘性 σ
                            //   4. 默认 σ
                            let (sigma, used_default, source, new_sticky) = if sigma_ema_prev > 0.01 {
                                let sig = sigma_ema_prev.clamp(smin, smax_poly);
                                (sig, false, "Poly IV (EMA)", sigma_ema_prev)
                            } else if steady && iv_raw > 0.01 {
                                let sig = iv_raw.clamp(smin, smax_poly);
                                (sig, false, "Poly IV (init)", iv_raw)
                            } else {
                                let sticky = s.sticky_volatility;
                                if sticky > 0.0 {
                                    (sticky, false, "粘性", sticky)
                                } else {
                                    (
                                        self.config.trading.default_volatility_annual,
                                        true,
                                        "默认",
                                        s.sticky_volatility,
                                    )
                                }
                            };

                            let mkt_state = if steady { "稳态" } else { "激变态" };
                            (
                                strike,
                                expiry_min,
                                sigma,
                                used_default,
                                source.to_string(),
                                mkt_state.to_string(),
                                new_sticky,
                                current_poly_p,
                                steady,
                                iv_raw,
                            )
                        } else {
                            (
                                96000.0,
                                5.0,
                                self.config.trading.default_volatility_annual,
                                true,
                                "默认".to_string(),
                                "—".to_string(),
                                0.0,
                                0.5,
                                false,
                                0.0,
                            )
                        };

                    // σ_ema 更新（闭包外，自由可变）：仅在稳态且 iv_raw 有效时喂 EMA，避免激变态污染
                    if steady && iv_raw_for_ema > 0.01 && iv_raw_for_ema < 5.0 {
                        if self.sigma_ema <= 0.0 {
                            self.sigma_ema = iv_raw_for_ema; // 首次初始化
                        } else {
                            self.sigma_ema = SIGMA_EMA_ALPHA * iv_raw_for_ema
                                + (1.0 - SIGMA_EMA_ALPHA) * self.sigma_ema;
                        }
                    }

                    let fair_p = bs_model::calculate_binary_call_price(
                        mid,
                        strike,
                        bs_model::minutes_to_years(expiry_min),
                        sigma,
                    );
                    let fair_down = (1.0 - fair_p).clamp(0.0, 1.0);

                    let iv_poly = bs_model::find_implied_volatility(
                        mid,
                        strike,
                        bs_model::minutes_to_years(expiry_min),
                        current_poly_p,
                    );

                    // FV 历史用于追涨「上涨」判定
                    self.fv_up_history.push_back(fair_p);
                    self.fv_down_history.push_back(fair_down);
                    while self.fv_up_history.len() > FV_HISTORY_LEN {
                        self.fv_up_history.pop_front();
                    }
                    while self.fv_down_history.len() > FV_HISTORY_LEN {
                        self.fv_down_history.pop_front();
                    }

                    let mut excited_snap: Option<(i64, SnapshotRow)> = None;
                    if let Ok(mut s) = self.state.write() {
                        s.best_bid = bba.bid;
                        s.best_ask = bba.ask;
                        s.mid_price = mid;
                        s.spread_bps = spread;
                        s.fair_price = fair_p;
                        s.fair_price_down = fair_down;
                        s.expiry_minutes = expiry_min;
                        s.volatility_annual = sigma;
                        s.sigma_used_default = sigma_used_default;
                        s.sigma_source = sigma_source;
                        s.market_state = market_state;
                        s.sticky_volatility = sticky_vol;
                        s.iv_poly = iv_poly;
                        let gap_bps_raw = (fair_p - current_poly_p).abs() / current_poly_p.max(0.02).max(1e-9) * 10000.0;
                        s.signal_gap_bps = gap_bps_raw.min(9999.0);

                        // 稳态→激变态且当前无 lead：开启一次领先，并尝试记录负延迟
                        if self.last_was_steady && !steady && s.excited_lead.is_none() {
                            let lead_start = Instant::now();
                            s.excited_lead = Some(ExcitedLead {
                                lead_start,
                                fv_lead: fair_p,
                                poly_mid_lead: current_poly_p,
                            });
                            // 负延迟：Poly 先动我们后判定；找最近一次同向且早于 lead_start 的 Poly 变动
                            let fv_dir = fair_p - current_poly_p;
                            if let Some((move_instant, move_mid)) = s
                                .recent_poly_moves
                                .iter()
                                .rev()
                                .find(|(inst, m)| {
                                    *inst < lead_start
                                        && (m - current_poly_p) * fv_dir >= 0.0
                                        && (m - current_poly_p).abs() >= POLY_MOVE_THRESH
                                })
                                .copied()
                            {
                                let neg_ms = (lead_start - move_instant).as_millis() as f64;
                                s.delay_stats.record_neg_ms(neg_ms);
                            }
                        }

                        // 激变态快照：按 5 分钟窗口写 CSV（Binance 事件）
                        if !steady && s.poly_window_end_ts > 0 {
                            excited_snap = Some((
                                s.poly_window_end_ts,
                                SnapshotRow {
                                    t_ms: chrono::Utc::now().timestamp_millis(),
                                    fv_up: fair_p,
                                    fv_down: fair_down,
                                    poly_up_bid: s.poly_best_bid,
                                    poly_up_ask: s.poly_best_ask,
                                    poly_down_bid: s.poly_down_best_bid,
                                    poly_down_ask: s.poly_down_best_ask,
                                    source: b'B',
                                },
                            ));
                        }

                    }

                    // 全量快照（v0.3.3）：每个 BookTicker 都落一行，毫秒级时间戳
                    let win_end = self
                        .state
                        .read()
                        .map(|s| s.poly_window_end_ts)
                        .unwrap_or(0);
                    if win_end > 0 {
                        let st_byte = if steady { b'S' } else { b'E' };
                        let (pub_, pua, pdb, pda) = self
                            .state
                            .read()
                            .map(|s| (s.poly_best_bid, s.poly_best_ask, s.poly_down_best_bid, s.poly_down_best_ask))
                            .unwrap_or((0.0, 0.0, 0.0, 0.0));
                        self.fv_snapshot_writer.push(
                            win_end,
                            FvRow {
                                t_ms: chrono::Utc::now().timestamp_millis(),
                                source: b'B',
                                binance_mid: mid,
                                fv_up: fair_p,
                                fv_down: fair_down,
                                poly_up_bid: pub_,
                                poly_up_ask: pua,
                                poly_down_bid: pdb,
                                poly_down_ask: pda,
                                sigma,
                                iv_raw: iv_raw_for_ema,
                                sigma_ema: self.sigma_ema,
                                market_state: st_byte,
                                expiry_min,
                                strike,
                            },
                        );
                    }
                    // 追涨侧与 Maker 买意图（在写锁外计算，避免 self 与 s 同时借用）
                    let (
                        poly_up_mid,
                        poly_down_mid,
                        up_ask,
                        down_ask,
                        _proj_avg_sum,
                        _proj_qty_up,
                        _proj_qty_down,
                        rebalance_buy_up,
                        rebalance_buy_down,
                        rebalance_qty_up,
                        rebalance_qty_down,
                        avg_after_up,
                        avg_after_down,
                    ) = if let Ok(s) = self.state.read() {
                        let up = if s.poly_best_bid > 0.0 && s.poly_best_ask > 0.0 {
                            (s.poly_best_bid + s.poly_best_ask) / 2.0
                        } else {
                            0.0
                        };
                        let down = if s.poly_down_best_bid > 0.0 && s.poly_down_best_ask > 0.0 {
                            (s.poly_down_best_bid + s.poly_down_best_ask) / 2.0
                        } else {
                            0.0
                        };
                        let proj_avg = s.projected_avg_sum_after_intents();
                        let proj_up = s.projected_qty_up_after_intents();
                        let proj_down = s.projected_qty_down_after_intents();
                        let up_ask = s.poly_best_ask;
                        let down_ask = s.poly_down_best_ask;
                        let price_up = (up_ask - TICK).max(0.0);
                        let price_down = (down_ask - TICK).max(0.0);
                        let rebalance_qty_up = (proj_down - proj_up).max(0.0);
                        let rebalance_qty_down = (proj_up - proj_down).max(0.0);
                        let avg_sum_after_rebalance_up = s.avg_sum_if_buy_filled(ChaseSide::Up, price_up, rebalance_qty_up);
                        let avg_sum_after_rebalance_down = s.avg_sum_if_buy_filled(ChaseSide::Down, price_down, rebalance_qty_down);
                        let rebalance_ok_up = proj_avg < 1.0 && rebalance_qty_up > 0.0 && avg_sum_after_rebalance_up < 1.0 && up_ask > 0.0;
                        let rebalance_ok_down = proj_avg < 1.0 && rebalance_qty_down > 0.0 && avg_sum_after_rebalance_down < 1.0 && down_ask > 0.0;
                        (
                            up,
                            down,
                            up_ask,
                            down_ask,
                            proj_avg,
                            proj_up,
                            proj_down,
                            rebalance_ok_up,
                            rebalance_ok_down,
                            rebalance_qty_up,
                            rebalance_qty_down,
                            avg_sum_after_rebalance_up,
                            avg_sum_after_rebalance_down,
                        )
                    } else {
                        (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, false, false, 0.0, 0.0, 0.0, 0.0)
                    };
                    // v0.4.5 taker-only：追涨/配平直接 taker 吃 best_ask，立即成交
                    //   底层逻辑：lead-lag 实证 1.1-2.5s alpha 必须 taker 才能抓到
                    //   maker 模式 fill 总在反转/卖压时（adverse selection），把 alpha 反转为劣势
                    let in_excited = !steady;
                    // v0.4.15: 双窗口守门 — 建仓腿严（≥1.5min），配平腿宽（≥0.5min）
                    let in_chase_window = expiry_min >= MIN_EXPIRY_MIN_FOR_CHASE;
                    let in_rebal_window = expiry_min >= MIN_EXPIRY_MIN_FOR_REBAL;
                    let up_chase = CHASE_ONLY_IN_EXCITED && in_excited && in_chase_window
                        && poly_up_mid > 0.0
                        && (fair_p - poly_up_mid) >= CHASE_GAP_MIN
                        && Self::fv_rising(&self.fv_up_history, fair_p);
                    let down_chase = CHASE_ONLY_IN_EXCITED && in_excited && in_chase_window
                        && poly_down_mid > 0.0
                        && (fair_down - poly_down_mid) >= CHASE_GAP_MIN
                        && Self::fv_rising(&self.fv_down_history, fair_down);
                    let chase_side = if up_chase {
                        Some(ChaseSide::Up)
                    } else if down_chase {
                        Some(ChaseSide::Down)
                    } else {
                        None
                    };
                    // Taker 吃单价 = best_ask（不再用 best_ask − 1c 的 maker 价）
                    let taker_price_up = up_ask;
                    let taker_price_down = down_ask;
                    // 守门 #1: 单边价上限 0.85（防尾部）
                    let price_up_ok = taker_price_up > 0.0 && taker_price_up <= MAX_BUY_PRICE;
                    let price_down_ok = taker_price_down > 0.0 && taker_price_down <= MAX_BUY_PRICE;
                    // v0.4.6 守门 #4 force-balance（E0）：只允许买"持仓 ≤ 对侧 + slack" 的方向
                    // v0.4.10 守门 #5 pair-health（C2 cost-aware）：fill 后 avg_sum 必须 < PAIR_HEALTH_MAX
                    let (qty_up_now, qty_down_now, avg_up_now, avg_down_now) = if let Ok(s) = self.state.read() {
                        (s.position_up.qty, s.position_down.qty,
                         s.position_up.avg_price, s.position_down.avg_price)
                    } else {
                        (0.0, 0.0, 0.0, 0.0)
                    };
                    let balance_ok_up = qty_up_now <= qty_down_now + FORCE_BALANCE_SLACK;
                    let balance_ok_down = qty_down_now <= qty_up_now + FORCE_BALANCE_SLACK;
                    // v0.4.14 双标准健康守门：建仓腿宽松 1.05（用 ask proxy），配平腿严格 0.98（防 avg_sum 漂移）
                    let qty_chase = MAKER_BUY_CHASE_MIN_QTY;
                    let new_avg_up_after = if qty_up_now + qty_chase > 0.0 {
                        (qty_up_now * avg_up_now + taker_price_up * qty_chase) / (qty_up_now + qty_chase)
                    } else { taker_price_up };
                    let new_avg_down_after = if qty_down_now + qty_chase > 0.0 {
                        (qty_down_now * avg_down_now + taker_price_down * qty_chase) / (qty_down_now + qty_chase)
                    } else { taker_price_down };
                    // 建仓腿（对侧空仓）：用 ask 当 proxy，宽松 < 1.05（容许 Polymarket spread 1c）
                    // 配平腿（对侧已有持仓）：用真实 avg，严格 < 0.98（确保 fill 后 avg_sum 不漂出健康区）
                    let pair_health_ok_up = if qty_down_now > 0.0 {
                        (new_avg_up_after + avg_down_now) < REBAL_HEALTH_MAX
                    } else {
                        let opp_proxy = if taker_price_down > 0.0 { taker_price_down } else { 0.5 };
                        (new_avg_up_after + opp_proxy) < PAIR_HEALTH_MAX
                    };
                    let pair_health_ok_down = if qty_up_now > 0.0 {
                        (avg_up_now + new_avg_down_after) < REBAL_HEALTH_MAX
                    } else {
                        let opp_proxy = if taker_price_up > 0.0 { taker_price_up } else { 0.5 };
                        (opp_proxy + new_avg_down_after) < PAIR_HEALTH_MAX
                    };
                    // v0.4.14 回归 v0.4.11 chase target — 删除 max(rebal) 抬高 target 的反向 bug
                    // 实证：max(chase, rebal) 让 best_ask 几乎总 ≤ target → 70% 走 taker（设计本意 maker）
                    // → 回归 target = FV - SAFETY_MARGIN，配平腿同样挂 maker 等价格跌到位
                    // 单向市挂不 fill 接受（5s timeout 无成本）；震荡市享 maker rebate
                    let target_up = (fair_p - SAFETY_MARGIN).max(0.0);
                    let target_down = (fair_down - SAFETY_MARGIN).max(0.0);
                    let now_ms = chrono::Utc::now().timestamp_millis();

                    // ① 先处理 pending 挂单：交给 OrderClient（dry-run: ExecutionSim；live: LiveOrderClient）
                    if let Ok(mut s) = self.state.write() {
                        let (up_fill, down_fill) =
                            self.order_client.tick(&mut s, target_up, target_down, now_ms);
                        if up_fill {
                            self.last_taker_up_ts_ms = now_ms;
                        }
                        if down_fill {
                            self.last_taker_down_ts_ms = now_ms;
                        }

                        // 稳态时清 chase_side（停 chase 显示）
                        if steady {
                            s.chase_side = None;
                        } else {
                            s.chase_side = chase_side;
                        }
                        s.rebalance_hint_up = if rebalance_buy_up { Some((rebalance_qty_up, avg_after_up)) } else { None };
                        s.rebalance_hint_down = if rebalance_buy_down { Some((rebalance_qty_down, avg_after_down)) } else { None };
                    }

                    // ② 决策是否新建仓 — 两条独立路径
                    // A) chase 路径（吃 lead alpha）：受激变态守门 + 节流；建仓腿只在激变 + lead 信号
                    // B) 偏仓配平路径（锁套利）：稳态也允许 + 不受节流；只要对侧已落后即触发
                    let imbalanced_up_needed = qty_down_now > qty_up_now;   // UP 落后，要补 UP
                    let imbalanced_dn_needed = qty_up_now > qty_down_now;   // DOWN 落后，要补 DOWN

                    // chase 触发：必须激变 + lead 信号 + force-balance + 1s 节流
                    let chase_buy_up = up_chase && balance_ok_up && in_excited
                        && now_ms - self.last_taker_up_ts_ms >= TAKER_BUY_INTERVAL_MS;
                    let chase_buy_down = down_chase && balance_ok_down && in_excited
                        && now_ms - self.last_taker_down_ts_ms >= TAKER_BUY_INTERVAL_MS;

                    // 偏仓配平：稳态也允许，无激变态守门，无节流（挂单慢动作）
                    // v0.4.15: 用 in_rebal_window (≥0.5min) — 末段仍允许配平救已建仓
                    let rebalance_buy_up_path = imbalanced_up_needed && in_rebal_window;
                    let rebalance_buy_down_path = imbalanced_dn_needed && in_rebal_window;

                    let trigger_up = chase_buy_up || rebalance_buy_up_path;
                    let trigger_down = chase_buy_down || rebalance_buy_down_path;

                    let want_buy_up = trigger_up
                        && pair_health_ok_up
                        && target_up > 0.0 && target_up <= MAX_BUY_PRICE;
                    let want_buy_down = trigger_down
                        && pair_health_ok_down
                        && target_down > 0.0 && target_down <= MAX_BUY_PRICE;

                    // ③ 派发 BuyIntent 到 OrderClient：marketable→FAK，否则 GTC；
                    //    live 模式下 best_ask≤target 时立刻更新节流（不等 WS fill）
                    if want_buy_up || want_buy_down {
                        if let Ok(mut s) = self.state.write() {
                            let up_token = s.poly_token_id.clone();
                            let down_token = s.poly_down_token_id.clone();
                            let (ua, da) = (s.poly_best_ask, s.poly_down_best_ask);
                            if want_buy_up {
                                let intent = BuyIntent {
                                    side: ChaseSide::Up,
                                    qty: MAKER_BUY_CHASE_MIN_QTY,
                                    target: target_up,
                                    reason: if chase_buy_up {
                                        PendingOrderReason::Chase
                                    } else {
                                        PendingOrderReason::Rebalance
                                    },
                                };
                                if ua > 0.0 && ua <= target_up {
                                    self.last_taker_up_ts_ms = now_ms;
                                }
                                self.order_client
                                    .dispatch_buy_intent(&intent, ua, &up_token, now_ms, &mut s);
                            }
                            if want_buy_down {
                                let intent = BuyIntent {
                                    side: ChaseSide::Down,
                                    qty: MAKER_BUY_CHASE_MIN_QTY,
                                    target: target_down,
                                    reason: if chase_buy_down {
                                        PendingOrderReason::Chase
                                    } else {
                                        PendingOrderReason::Rebalance
                                    },
                                };
                                if da > 0.0 && da <= target_down {
                                    self.last_taker_down_ts_ms = now_ms;
                                }
                                self.order_client
                                    .dispatch_buy_intent(&intent, da, &down_token, now_ms, &mut s);
                            }
                        }
                    }
                    if let Some((win, row)) = excited_snap {
                        self.excited_snapshot_writer.push_snapshot(win, row);
                    }
                    self.last_was_steady = steady;
                }
            }

            // ── Depth Update ─────────────────────────────────────────
            MarketEvent::Depth { data, .. } => match self.orderbook.apply_depth_update(&data) {
                Ok(_) if self.orderbook.is_ready() => {
                    if let Ok(mut s) = self.state.write() {
                        s.bids = self
                            .orderbook
                            .top_bids(10)
                            .into_iter()
                            .map(|(p, q)| BookLevel { price: p, qty: q })
                            .collect();
                        s.asks = self
                            .orderbook
                            .top_asks(10)
                            .into_iter()
                            .map(|(p, q)| BookLevel { price: p, qty: q })
                            .collect();
                    }
                }
                Err(e) if e.to_string() == "orderbookgap" => {
                    warn!("订单簿不连续，已重置");
                }
                _ => {}
            },

            // ── AggTrade ─────────────────────────────────────────────
            MarketEvent::AggTrade { data, .. } => {
                if let Ok(trade) = Trade::try_from(&data) {
                    let is_buy = trade.is_taker_buy();
                    let dir = if is_buy { "▲" } else { "▼" };
                    
                    // 更新滚动波动率（仅喂价与时间戳；年化 σ 在 BookTicker 中按到期 T 对齐计算）
                    self.vol_calc.update(trade.price, trade.trade_time_ms);

                    // 计算网络延迟 (ms) = 本机接收时间 - 交易所成交时间
                    let now_ms = (Local::now().timestamp_nanos_opt().unwrap_or(0) / 1_000_000) as u64;
                    let net_latency = now_ms.saturating_sub(trade.trade_time_ms);

                    let row = TradeRow {
                        dir_time: format!("{} {}", dir, Local::now().format("%H:%M:%S")),
                        is_buy,
                        price: trade.price,
                        qty: trade.quantity,
                        notional: trade.notional(),
                    };
                    if let Ok(mut s) = self.state.write() {
                        let s: &mut AppState = &mut *s;
                        s.push_trade(row);
                        // 更新延迟展示数据
                        s.latency.p50_ms = net_latency as f64; // 简化处理，直接显示当前值
                    }
                }
            }

            MarketEvent::PolyBookUpdate { .. } => {
                let mut excited_snap: Option<(i64, SnapshotRow)> = None;
                let mut fv_snap_pending: Option<(i64, FvRow)> = None;
                if let Ok(mut s) = self.state.write() {
                    // 与 BookTicker 一致：用 Up 中价（有 Up 用 Up，无则用 1 - Down）
                    let poly_mid = {
                        let up = s.poly_best_bid > 0.0 && s.poly_best_ask > 0.0;
                        let down = s.poly_down_best_bid > 0.0 && s.poly_down_best_ask > 0.0;
                        if up {
                            (s.poly_best_bid + s.poly_best_ask) / 2.0
                        } else if down {
                            1.0 - (s.poly_down_best_bid + s.poly_down_best_ask) / 2.0
                        } else {
                            s.last_poly_mid
                        }
                    };
                    s.poly_btc_offset = poly_mid - s.mid_price;

                    // 近期 Poly 明显变动（保留约 5s，用于负延迟）
                    let five_sec = Duration::from_secs(5);
                    while s
                        .recent_poly_moves
                        .front()
                        .map(|(i, _)| i.elapsed() > five_sec)
                        .unwrap_or(false)
                    {
                        s.recent_poly_moves.pop_front();
                    }
                    if (poly_mid - s.last_poly_mid).abs() >= POLY_MOVE_THRESH && s.last_poly_mid > 0.0 {
                        s.recent_poly_moves.push_back((Instant::now(), poly_mid));
                    }
                    s.last_poly_mid = poly_mid;

                    // 进行中的 lead：检查跟上或超时
                    if let Some(lead) = s.excited_lead.as_ref() {
                        let elapsed_ms = lead.lead_start.elapsed().as_millis();
                        if elapsed_ms > LEAD_TIMEOUT_MS as u128 {
                            s.excited_lead = None;
                        } else if (poly_mid - lead.fv_lead).abs() < CATCHUP_EPSILON {
                            let delay_ms = lead.lead_start.elapsed().as_millis() as f64;
                            s.delay_stats.record_pos_ms(delay_ms);
                            s.excited_lead = None;
                        }
                    }

                    // 激变态快照（Poly 事件）：按 5 分钟窗口写 CSV
                    if s.market_state == "激变态" && s.poly_window_end_ts > 0 {
                        excited_snap = Some((
                            s.poly_window_end_ts,
                            SnapshotRow {
                                t_ms: chrono::Utc::now().timestamp_millis(),
                                fv_up: s.fair_price,
                                fv_down: s.fair_price_down,
                                poly_up_bid: s.poly_best_bid,
                                poly_up_ask: s.poly_best_ask,
                                poly_down_bid: s.poly_down_best_bid,
                                poly_down_ask: s.poly_down_best_ask,
                                source: b'P',
                            },
                        ));
                    }

                    // Poly 盘口变动也驱动 OrderClient 推进 pending 挂单（fill / ttl）；
                    // 传 0.0 目标价 → 跳过 reprice 检查（reprice 仅 BookTicker 决策帧触发）
                    let ts_ms = chrono::Utc::now().timestamp_millis();
                    let (up_fill, down_fill) =
                        self.order_client.tick(&mut s, 0.0, 0.0, ts_ms);
                    if up_fill {
                        self.last_taker_up_ts_ms = ts_ms;
                    }
                    if down_fill {
                        self.last_taker_down_ts_ms = ts_ms;
                    }

                    // v0.4.0-5m：虚拟 merge 触发（替代旧浮亏 sell 减仓）
                    //   底层逻辑：1 UP + 1 DOWN ≡ 1 USDC（Polymarket CTF mergePositions 任意时刻可调）
                    //   触发条件：可配对张数 ≥ MERGE_TRIGGER_PAIR_QTY 且距上次 merge ≥ MERGE_INTERVAL_MS
                    //   v0.4.1 P0 守门 #3：avg_sum > 1 时拒绝 merge（避免主动锁亏，等结算 redeem 兜底）
                    //   v0.4.2 P1.2：窗口末段（expiry_min < FORCE_MERGE_EXPIRY_MIN）强制 merge：
                    //     - 每对 merge=$1.00 与 redeem 数学等价
                    //     - 即时锁定避免残仓过夜 gas + 清算时机风险
                    //     - 强制忽略 avg_sum<1 守门（窗口末段没机会等改善）
                    let mergeable = s.mergeable_pairs();
                    let merge_throttle_ok = ts_ms - s.last_merge_ts_ms >= MERGE_INTERVAL_MS;
                    let avg_sum_now = s.position_up.avg_price + s.position_down.avg_price;
                    let force_merge = s.expiry_minutes < FORCE_MERGE_EXPIRY_MIN;
                    let merge_avg_sum_ok = force_merge || avg_sum_now < MERGE_AVG_SUM_MAX;
                    if mergeable >= MERGE_TRIGGER_PAIR_QTY && merge_throttle_ok && merge_avg_sum_ok {
                        let pair_qty = mergeable.floor().max(MERGE_TRIGGER_PAIR_QTY);
                        s.apply_merge(pair_qty, ts_ms);
                    }
                    // 全量快照（v0.3.3）：每个 PolyBookUpdate 都落一行
                    if s.poly_window_end_ts > 0 {
                        let st_byte = if s.market_state == "稳态" { b'S' } else { b'E' };
                        fv_snap_pending = Some((
                            s.poly_window_end_ts,
                            FvRow {
                                t_ms: chrono::Utc::now().timestamp_millis(),
                                source: b'P',
                                binance_mid: s.mid_price,
                                fv_up: s.fair_price,
                                fv_down: s.fair_price_down,
                                poly_up_bid: s.poly_best_bid,
                                poly_up_ask: s.poly_best_ask,
                                poly_down_bid: s.poly_down_best_bid,
                                poly_down_ask: s.poly_down_best_ask,
                                sigma: s.volatility_annual,
                                iv_raw: s.iv_poly,
                                sigma_ema: self.sigma_ema,
                                market_state: st_byte,
                                expiry_min: s.expiry_minutes,
                                strike: s.strike_price,
                            },
                        ));
                    }
                }
                if let Some((win, row)) = excited_snap {
                    self.excited_snapshot_writer.push_snapshot(win, row);
                }
                if let Some((win, row)) = fv_snap_pending {
                    self.fv_snapshot_writer.push(win, row);
                }
            }

            MarketEvent::Unknown { .. } => {}
        }
    }
}
