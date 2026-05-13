use chrono::Local;
/// strategy/signal.rs - 策略引擎
/// 消费 MarketEvent，将数据写入 AppState 供 TUI 展示（无下单/撤单逻辑）
/// 实现文档：Poly IV + 稳态/激变态 + 粘性波动率
use std::collections::VecDeque;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tracing::warn;

use crate::{
    config::AppConfig,
    execution::sim::ExecutionSim,
    model::{orderbook::LocalOrderBook, ticker::BestBidAsk, trade::Trade},
    strategy::{
        decision::{self, MarketSnapshot, Thresholds},
        excited_snapshots::{ExcitedSnapshotWriter, SnapshotRow},
        fv::{FvEngine, FvInputs},
        fv_snapshots::{FvRow, FvSnapshotWriter},
        volatility::RollingVolatility,
    },
    tui::app::{AppState, BookLevel, ExcitedLead, TradeRow},
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
/// Poly 最小报价单位（1 美分）
const TICK: f64 = 0.01;
/// 追涨侧 Maker 买挂单数量（张）—— Polymarket 强制 5 张最低，此处用 100 张作资金体量
const MAKER_BUY_CHASE_MIN_QTY: f64 = 100.0;
/// Polymarket 最低订单数量（张）。配平缺口低于该值时跳过，避免实盘拒单。
const MIN_ORDER_QTY: f64 = 5.0;
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
/// Origin-compatible fill mode：挂单超过此时间未 fill → cancel。
const MAKER_TIMEOUT_MS: i64 = 5000;
/// v0.4.15 IOC walk-the-book：chase 腿允许吃簿到 target + 此滑点（cent）。
/// 上限来自 CHASE_GAP_MIN(10c) - SAFETY_MARGIN(5c) = 5c 容差，砍一半给未来反弹空间。
const CHASE_WORST_SLIPPAGE: f64 = 0.03;
/// v0.4.15 IOC walk-the-book：配平腿 worst = (1 - opp.avg) - 此预算。
/// 与 REBAL_HEALTH_MAX=0.98 严格对齐：吃完后 avg_sum < 0.98，每对 merge 至少锁 2c 利润。
const REBAL_WORST_HEAD_ROOM: f64 = 0.02;
/// FV 移动 ≥ 此 tick 数时视为目标已变化，IOC 评估不再走旧 target（仅观察用）。
const REPRICE_TICK_THRESH: f64 = 0.02;
/// 距窗口结束 < 此分钟数时停止新建 chase 仓。
/// v0.4.15: 0.5 → 1.5，避免末段建仓导致单边卡死。
const MIN_EXPIRY_MIN_FOR_CHASE: f64 = 1.5;
/// 距窗口结束 < 此分钟数时配平腿仍允许，拯救已建仓偏仓。
const MIN_EXPIRY_MIN_FOR_REBAL: f64 = 0.5;
/// Poly WS 数据新鲜度守门：距上次 poly 事件超过此毫秒则禁止建仓。
/// 1500ms 来自 lead P95=2.4s；Poly 静默超 1.5s 说明 WS 中断或 token 未订阅，
/// 基于陈旧 best 价走簿会打穿 worst_price 拿到错误 fill。
const POLY_STALE_MS: u64 = 1500;
/// Merge 守门：avg_sum > 1.0 + EPS 时拒绝 merge（避免主动锁亏）。
/// 等价于"配错方向后等待 redeem 而非主动 merge 锁定亏损"。
const MERGE_AVG_SUM_MAX: f64 = 1.0;
/// P1.2：窗口末段强制 merge 阈值。expiry_min < 此值时绕过 MERGE_AVG_SUM_MAX 守门，
/// 全量 merge 已配对部分。底层逻辑：每对 merge=$1.00，与 redeem 数学等价；
/// 但 merge 即时锁定，避免残仓过夜的链上 gas + 时机风险。
const FORCE_MERGE_EXPIRY_MIN: f64 = 1.0;

/// IV EMA 平滑系数 α。每帧反解出的 IV 加权 α，旧值加权 (1-α)。
/// α=0.02 ≈ 时间常数 50 帧 ≈ 5s @ 100ms/tick。让 σ 反映市场共识但不被 Binance 瞬时跳动吸收。
///
/// 已迁移到 strategy::fv 模块，此处保留常量占位避免 grep 漏网。

pub struct SignalEngine {
    config: AppConfig,
    orderbook: LocalOrderBook,
    state: Arc<RwLock<AppState>>,
    vol_calc: RollingVolatility,
    /// 币安 mid 历史 (ts_ms, mid)，用于稳态/激变态判定，保留约 5 秒
    binance_mid_history: VecDeque<(u64, f64)>,
    /// 上一帧是否为稳态（用于检测 稳态→激变态 开启 lead）
    last_was_steady: bool,
    /// 激变态快照：按窗口写 CSV，保留最近 10 个 5 分钟市场
    excited_snapshot_writer: ExcitedSnapshotWriter,
    /// FV 引擎：托管 σ_ema、FV 历史，封装 BS 反解+定价
    fv_engine: FvEngine,
    /// 全量快照采集器（v0.3.3）：每个 BookTicker / PolyBookUpdate 都落盘
    fv_snapshot_writer: FvSnapshotWriter,
    /// v0.4.5: 同侧 taker buy 节流时间戳
    last_taker_up_ts_ms: i64,
    last_taker_down_ts_ms: i64,
    /// Dry-run execution simulator. Strategy stays origin/main-compatible; this owns fill realism.
    execution_sim: ExecutionSim,
}

impl SignalEngine {
    pub fn new(config: AppConfig, state: Arc<RwLock<AppState>>) -> Self {
        let depth_limit = config.orderbook.depth_levels;
        let symbol = config.trading.symbol.clone();
        Self {
            config,
            orderbook: LocalOrderBook::new(&symbol, depth_limit),
            state,
            vol_calc: RollingVolatility::new(500),
            binance_mid_history: VecDeque::with_capacity(200),
            last_was_steady: false,
            excited_snapshot_writer: ExcitedSnapshotWriter::new(),
            fv_engine: FvEngine::new(),
            fv_snapshot_writer: FvSnapshotWriter::new(),
            last_taker_up_ts_ms: 0,
            last_taker_down_ts_ms: 0,
            execution_sim: ExecutionSim::new(60, CANCEL_DELAY_MS, MAKER_TIMEOUT_MS),
        }
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
            MarketEvent::BookTicker { data, .. } => self.handle_book_ticker(data),
            MarketEvent::Depth { data, .. } => self.handle_depth(data),
            MarketEvent::AggTrade { data, .. } => self.handle_agg_trade(data),
            MarketEvent::PolyBookUpdate { .. } => self.handle_poly_book_update(),
            MarketEvent::Unknown { .. } => {}
        }
    }

    /// Binance BookTicker → FV 重算 + decision + IOC 执行 + 全量快照落盘
    fn handle_book_ticker(&mut self, data: crate::model::ticker::BookTickerData) {
        if let Ok(bba) = BestBidAsk::try_from(&data) {
            let spread = bba.spread_bps();
            let mid = bba.mid_price();
            let now_ts_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;

            // 更新币安 mid 历史，用于稳态/激变态判定
            self.binance_mid_history.push_back((now_ts_ms, mid));
            let cutoff_5s = now_ts_ms.saturating_sub(5000);
            while self
                .binance_mid_history
                .front()
                .map(|&(t, _)| t < cutoff_5s)
                .unwrap_or(false)
            {
                self.binance_mid_history.pop_front();
            }

            // 步骤 A：从 state 抽出 FV 计算所需的最小输入；释放读锁后再交给 FvEngine
            let (strike, expiry_min, current_poly_p, has_poly, steady, sticky_in) = if let Ok(s) =
                self.state.read()
            {
                let now_ts = chrono::Utc::now().timestamp();
                let expiry_min = if s.poly_window_end_ts <= 0 {
                    5.0
                } else {
                    ((s.poly_window_end_ts - now_ts) as f64 / 60.0).max(0.0)
                };
                let strike = if s.strike_price > 0.0 {
                    s.strike_price
                } else {
                    mid.round()
                };
                let up_valid = s.poly_best_bid > 0.0 && s.poly_best_ask > 0.0;
                let down_valid = s.poly_down_best_bid > 0.0 && s.poly_down_best_ask > 0.0;
                let up_mid = up_valid.then(|| {
                    let (lo, hi) = (
                        s.poly_best_bid.min(s.poly_best_ask),
                        s.poly_best_bid.max(s.poly_best_ask),
                    );
                    (lo + hi) / 2.0
                });
                let down_mid = down_valid.then(|| {
                    let (lo, hi) = (
                        s.poly_down_best_bid.min(s.poly_down_best_ask),
                        s.poly_down_best_bid.max(s.poly_down_best_ask),
                    );
                    (lo + hi) / 2.0
                });
                let current_poly_p = up_mid
                    .unwrap_or_else(|| down_mid.map(|d| (1.0 - d).clamp(0.0, 1.0)).unwrap_or(0.5));

                let (move_5s, move_1s) = self.move_range(now_ts_ms);
                let (poly_spread, has_poly) = if up_valid {
                    ((s.poly_best_ask - s.poly_best_bid).max(0.0), true)
                } else if down_valid {
                    ((s.poly_down_best_ask - s.poly_down_best_bid).max(0.0), true)
                } else {
                    (1.0, false)
                };
                let poly_spread_narrow = poly_spread < POLY_SPREAD_NARROW_MAX;
                let steady = move_5s < STEADY_MOVE_5S_MAX
                    && move_1s < EXCITED_MOVE_1S_MIN
                    && has_poly
                    && poly_spread_narrow;
                (
                    strike,
                    expiry_min,
                    current_poly_p,
                    has_poly,
                    steady,
                    s.sticky_volatility,
                )
            } else {
                (96000.0, 5.0, 0.5, false, false, 0.0)
            };
            let market_state = if steady { "稳态" } else { "激变态" }.to_string();

            // 步骤 B：FvEngine 一次性给出 σ / FV / iv_poly / FV 上涨判定
            let fv = self.fv_engine.compute(FvInputs {
                binance_mid: mid,
                strike,
                expiry_min,
                poly_p: current_poly_p,
                has_poly,
                steady,
                sticky_sigma: sticky_in,
                sigma_min: self.config.trading.volatility_sigma_min,
                sigma_max_poly: self.config.trading.volatility_sigma_max_poly,
                default_sigma: self.config.trading.default_volatility_annual,
            });
            let fair_p = fv.fair_up;
            let fair_down = fv.fair_down;
            let sigma = fv.sigma;
            let sigma_used_default = fv.sigma_used_default;
            let sigma_source = fv.sigma_source.to_string();
            let sticky_vol = fv.new_sticky;
            let iv_raw_for_ema = fv.iv_raw;
            let iv_poly = fv.iv_poly;

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
                let gap_bps_raw =
                    (fair_p - current_poly_p).abs() / current_poly_p.max(0.02).max(1e-9) * 10000.0;
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
            let win_end = self.state.read().map(|s| s.poly_window_end_ts).unwrap_or(0);
            if win_end > 0 {
                let st_byte = if steady { b'S' } else { b'E' };
                let (pub_, pua, pdb, pda) = self
                    .state
                    .read()
                    .map(|s| {
                        (
                            s.poly_best_bid,
                            s.poly_best_ask,
                            s.poly_down_best_bid,
                            s.poly_down_best_ask,
                        )
                    })
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
                        sigma_ema: self.fv_engine.sigma_ema(),
                        market_state: st_byte,
                        expiry_min,
                        strike,
                    },
                );
            }

            let now_ms = chrono::Utc::now().timestamp_millis();
            if let Ok(mut s) = self.state.write() {
                let fills = self.execution_sim.process(
                    &mut s,
                    (fair_p - SAFETY_MARGIN).max(0.0),
                    (fair_down - SAFETY_MARGIN).max(0.0),
                    now_ms,
                );
                if fills.up {
                    self.last_taker_up_ts_ms = now_ms;
                }
                if fills.down {
                    self.last_taker_down_ts_ms = now_ms;
                }
            }

            // 步骤 C：一次 read 装填 decision 所需的市场快照
            let snap = if let Ok(s) = self.state.read() {
                let up_mid = if s.poly_best_bid > 0.0 && s.poly_best_ask > 0.0 {
                    (s.poly_best_bid + s.poly_best_ask) / 2.0
                } else {
                    0.0
                };
                let down_mid = if s.poly_down_best_bid > 0.0 && s.poly_down_best_ask > 0.0 {
                    (s.poly_down_best_bid + s.poly_down_best_ask) / 2.0
                } else {
                    0.0
                };
                let poly_fresh = s
                    .poly_delay_ms()
                    .map(|ms| ms <= POLY_STALE_MS)
                    .unwrap_or(false);
                MarketSnapshot {
                    poly_up_mid: up_mid,
                    poly_down_mid: down_mid,
                    up_ask: s.poly_best_ask,
                    down_ask: s.poly_down_best_ask,
                    qty_up: s.ledger.position_up.qty,
                    qty_down: s.ledger.position_down.qty,
                    avg_up: s.ledger.position_up.avg_price,
                    avg_down: s.ledger.position_down.avg_price,
                    steady,
                    expiry_min,
                    poly_fresh,
                }
            } else {
                MarketSnapshot {
                    poly_up_mid: 0.0,
                    poly_down_mid: 0.0,
                    up_ask: 0.0,
                    down_ask: 0.0,
                    qty_up: 0.0,
                    qty_down: 0.0,
                    avg_up: 0.0,
                    avg_down: 0.0,
                    steady,
                    expiry_min,
                    poly_fresh: false,
                }
            };

            // 步骤 D：纯函数 decision::build_intents → 0~2 笔 BuyIntent + 展示副作用
            let thresholds = Thresholds {
                chase_gap_min: CHASE_GAP_MIN,
                safety_margin: SAFETY_MARGIN,
                chase_worst_slippage: CHASE_WORST_SLIPPAGE,
                rebal_worst_head_room: REBAL_WORST_HEAD_ROOM,
                pair_health_max: PAIR_HEALTH_MAX,
                rebal_health_max: REBAL_HEALTH_MAX,
                max_buy_price: MAX_BUY_PRICE,
                chase_min_qty: MAKER_BUY_CHASE_MIN_QTY,
                min_order_qty: MIN_ORDER_QTY,
                min_expiry_min_for_chase: MIN_EXPIRY_MIN_FOR_CHASE,
                min_expiry_min_for_rebal: MIN_EXPIRY_MIN_FOR_REBAL,
                taker_buy_interval_ms: TAKER_BUY_INTERVAL_MS,
                force_balance_slack: FORCE_BALANCE_SLACK,
                chase_only_in_excited: CHASE_ONLY_IN_EXCITED,
            };
            let (intents, effects) = decision::build_intents(
                &fv,
                &snap,
                &thresholds,
                now_ms,
                self.last_taker_up_ts_ms,
                self.last_taker_down_ts_ms,
            );

            // 步骤 E：写展示副作用 + submit intent to execution simulator.
            // 决策逻辑与 origin/main 对齐；成交由延迟/队列模拟器推进，避免同步假 fill。
            if let Ok(mut s) = self.state.write() {
                s.chase_side = effects.chase_side;
                s.ledger.rebalance_hint_up = effects.rebalance_hint_up;
                s.ledger.rebalance_hint_down = effects.rebalance_hint_down;
                for intent in &intents {
                    self.execution_sim.submit_buy_intent(&mut s, intent, now_ms);
                }
            }
            if let Some((win, row)) = excited_snap {
                self.excited_snapshot_writer.push_snapshot(win, row);
            }
            self.last_was_steady = steady;
        }
    }

    /// Binance L2 增量 → 本地 orderbook + AppState top10 档位
    fn handle_depth(&mut self, data: crate::model::orderbook::DepthData) {
        match self.orderbook.apply_depth_update(&data) {
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
        }
    }

    /// Binance AggTrade → 喂滚动波动率 + 推 trade row 给 TUI
    fn handle_agg_trade(&mut self, data: crate::model::trade::AggTradeData) {
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

    /// Poly CLOB 更新 → 更新 offset / 跟上检查 / Merge 触发 / 快照落盘
    fn handle_poly_book_update(&mut self) {
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

            // Maker 买模拟成交：我们挂买单，成交 = 卖盘跌到我们挂买价或以下（有人卖到我们这一档），即 best_ask <= 挂单价
            let ts_ms = chrono::Utc::now().timestamp_millis();
            let target_up = (s.fair_price - SAFETY_MARGIN).max(0.0);
            let target_down = (s.fair_price_down - SAFETY_MARGIN).max(0.0);
            let fills = self
                .execution_sim
                .process(&mut s, target_up, target_down, ts_ms);
            if fills.up {
                self.last_taker_up_ts_ms = ts_ms;
            }
            if fills.down {
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
            let merge_throttle_ok = ts_ms - s.ledger.last_merge_ts_ms >= MERGE_INTERVAL_MS;
            let avg_sum_now = s.ledger.position_up.avg_price + s.ledger.position_down.avg_price;
            let force_merge = s.expiry_minutes < FORCE_MERGE_EXPIRY_MIN;
            let merge_avg_sum_ok = force_merge || avg_sum_now < MERGE_AVG_SUM_MAX;
            if mergeable >= MERGE_TRIGGER_PAIR_QTY && merge_throttle_ok && merge_avg_sum_ok {
                let pair_qty = mergeable.floor().max(MERGE_TRIGGER_PAIR_QTY);
                s.apply_merge(pair_qty, ts_ms);
            }
            // 全量快照（v0.3.3）：每个 PolyBookUpdate 都落一行
            if s.poly_window_end_ts > 0 {
                let st_byte = if s.market_state == "稳态" {
                    b'S'
                } else {
                    b'E'
                };
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
                        sigma_ema: self.fv_engine.sigma_ema(),
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
}
