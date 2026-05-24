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
    execution::client::OrderClient,
    model::{orderbook::LocalOrderBook, ticker::BestBidAsk, trade::Trade},
    position::PositionSide,
    strategy::{
        decision::{self, MarketSnapshot, Thresholds},
        excited_snapshots::{ExcitedSnapshotWriter, SnapshotRow},
        fv::{FvEngine, FvInputs},
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
/// Polymarket 最低订单数量（张）。
const MIN_ORDER_QTY: f64 = 5.0;
/// Merge 触发：可配对张数 ≥ 此值时进行虚拟 merge
const MERGE_TRIGGER_PAIR_QTY: f64 = 1.0;
/// Merge 节流：两次 merge 至少间隔此毫秒数
const MERGE_INTERVAL_MS: i64 = 1000;

/// 同侧 taker buy 最小间隔（jump 触发后 1s 内不重复下单）
const TAKER_BUY_INTERVAL_MS: i64 = 1000;

/// 单边买入价格上限（建仓守门）。
/// > 0.85 价位的"几乎确定"赌局风险收益不对称（1c edge vs 49:1 损失）。
const MAX_BUY_PRICE: f64 = 0.85;

/// 健康对守门（建仓腿对侧空仓）：fill 后 avg_sum < 此值才允许下单。
/// 1.02 = REBAL_HEALTH_MAX(0.98) + 4c spread+fee buffer。
/// 末段（< 1 min）由 decision.rs 动态放宽到 1.20，避免赢家方贵时配不上。
const PAIR_HEALTH_MAX: f64 = 1.02;

/// JumpChase 触发：1s 内 binance mid 变化 ≥ 此美元数。
/// v0.6 改为 $15（原 $30）：calm market 下 $30 一晚 0 触发。降到 $15 有足够频率。
const JUMP_USD_THRESHOLD: f64 = 15.0;
/// JumpChase 回看窗口：取 binance_mid_history 过去这个毫秒内的最早样本作起点。
const JUMP_LOOKBACK_MS: i64 = 1_000;
/// JumpChase confirm 窗口：触发后这么久仍可下单。与 spot_s jump-correction 5s 衰减对齐。
const JUMP_CONFIRM_MS: i64 = 5_000;
/// JumpChase 单笔基础张数。15 × 0.65 = $9.75 单笔最大损失。
const JUMP_CHASE_QTY: f64 = 15.0;
/// jump_window 下界：距窗口末 < 此分钟数时禁 jump。
const MIN_EXPIRY_MIN_FOR_CHASE: f64 = 1.5;
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
    /// 同侧 taker buy 节流时间戳；marketable BuyIntent / sim fill 后更新，供 decision 1s 节流读取。
    last_taker_up_ts_ms: i64,
    last_taker_down_ts_ms: i64,
    /// 干跑由 ExecutionSim impl，实盘由 LiveOrderClient impl。
    order_client: Arc<dyn OrderClient>,
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
            vol_calc: RollingVolatility::new(500),
            binance_mid_history: VecDeque::with_capacity(200),
            last_was_steady: false,
            excited_snapshot_writer: ExcitedSnapshotWriter::new(),
            fv_engine: FvEngine::new(),
            fv_snapshot_writer: FvSnapshotWriter::new(),
            last_taker_up_ts_ms: 0,
            last_taker_down_ts_ms: 0,
            order_client,
        }
    }

    /// 过去 1 秒 binance mid 的方向性变化（带符号；正=最近上涨，负=下跌）。
    /// 取 1s 前的最早 sample 到现在的最新 sample 差值。用于 JumpChase 触发：
    /// |Δmid_1s| ≥ JUMP_USD_THRESHOLD 即认为是猛跳事件。
    fn jump_1s(&self, now_ts_ms: u64) -> f64 {
        let cutoff = now_ts_ms.saturating_sub(JUMP_LOOKBACK_MS as u64);
        let mut earliest_in_window: Option<f64> = None;
        let mut latest: Option<f64> = None;
        for &(t, m) in &self.binance_mid_history {
            if t >= cutoff {
                if earliest_in_window.is_none() {
                    earliest_in_window = Some(m);
                }
                latest = Some(m);
            }
        }
        match (earliest_in_window, latest) {
            (Some(start), Some(end)) => end - start,
            _ => 0.0,
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
            MarketEvent::ChainlinkPrice { data, .. } => self.handle_chainlink_price(data),
            MarketEvent::Unknown { .. } => {}
        }
    }

    /// Chainlink Data Streams 推价 → 写 AppState + 更新 basis + 维护 chainlink history。
    /// 当前实现：仅刷新状态，不主动触发 FV 重算（FV 在每 BookTicker 重算时会读最新 chainlink）。
    fn handle_chainlink_price(
        &mut self,
        data: crate::model::chainlink::ChainlinkPriceData,
    ) {
        if let Ok(mut s) = self.state.write() {
            let now_ms = chrono::Utc::now().timestamp_millis();
            s.chainlink_price = Some(data.price);
            s.chainlink_obs_ts = Some(data.observation_ts);
            s.chainlink_recv_ts_ms = Some(now_ms);
            // 计算 basis（仅当 binance mid 已就绪）
            if s.mid_price > 0.0 {
                s.basis_vs_chainlink = Some(s.mid_price - data.price);
            }
        }
    }

    /// Binance BookTicker → FV 重算 + decision + IOC 执行 + 全量快照落盘
    fn handle_book_ticker(&mut self, data: crate::model::ticker::BookTickerData) {
        if let Ok(bba) = BestBidAsk::try_from(&data) {
            let spread = bba.spread_bps();
            let mid = bba.mid_price();
            let now_ts_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;

            // 更新币安 mid 历史，用于稳态/激变态判定 + JumpChase 触发器
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

            // JumpChase 检测：1s 内 binance mid 变化 ≥ $30 → 抢跑（chainlink 5s 内会跟 ~126%）
            let jump_1s = self.jump_1s(now_ts_ms);
            let now_ms_i64 = now_ts_ms as i64;
            if jump_1s.abs() >= JUMP_USD_THRESHOLD {
                let dir: i8 = if jump_1s > 0.0 { 1 } else { -1 };
                self.last_taker_up_ts_ms = self.last_taker_up_ts_ms; // 防 unused warning
                if let Ok(mut s) = self.state.write() {
                    s.binance_jump_1s = jump_1s;
                    s.last_jump_ts_ms = Some(now_ms_i64);
                    s.last_jump_dir = Some(dir);
                }
            } else if let Ok(mut s) = self.state.write() {
                // 衰减：没新事件时把当前 jump_1s 实时刷新（用于 TUI 显示）
                s.binance_jump_1s = jump_1s;
            }

            // 步骤 A：从 state 抽出 FV 计算所需的最小输入；释放读锁后再交给 FvEngine。
            // v0.5.0: 同步取 chainlink 价（结算源）；若可用则作为 S 优先于 binance mid。
            // v0.5.3: 同步取 Deribit smile（前瞻 σ 源）。
            let (
                strike,
                expiry_min,
                current_poly_p,
                has_poly,
                steady,
                sticky_in,
                _chainlink_s,
                deribit_surface,
            ) = if let Ok(s) = self.state.read()
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
                    s.chainlink_price,
                    s.deribit_surface.clone(),
                )
            } else {
                (96000.0, 5.0, 0.5, false, false, 0.0, None, None)
            };
            let market_state = if steady { "稳态" } else { "激变态" }.to_string();
            // 现价 S：chainlink + 5s-windowed jump correction（v0.5.4）
            //
            // 教训：v0.5.2 让 S = binance mid 是错的。
            // bi_cl_response.py 数据说 chainlink 5s 内追 binance ±$30 jump ~126%，
            // 但**这是增量响应、不是绝对基差闭合**。$80 basis 持续几小时不收敛。
            // 静态用 binance 当 S 等于赌 "5min 内 chainlink 一定追到 binance"，无依据。
            //
            // 正确用法：领先优势只在 jump 后 5s 窗口内可信
            //   静态：spot_s = chainlink_price                    （结算价真值）
            //   Jump 后 0-5s：spot_s = chainlink + jump_delta × decay
            //   5s 后：spot_s = chainlink                         （decay 到 0）
            let now_ms_for_jump = chrono::Utc::now().timestamp_millis();
            let chainlink_base = _chainlink_s.unwrap_or(mid);
            const JUMP_WINDOW_MS: i64 = 5_000;
            const JUMP_PASS_THROUGH: f64 = 1.26; // 实测 5s 跟随系数
            let jump_correction = match self.state.read() {
                Ok(s) => match s.last_jump_ts_ms {
                    Some(jump_ts) => {
                        let elapsed = now_ms_for_jump - jump_ts;
                        if elapsed >= 0 && elapsed < JUMP_WINDOW_MS {
                            // 时间从 0 到 5s 线性 decay：1.0 → 0.0
                            let remaining = 1.0 - (elapsed as f64 / JUMP_WINDOW_MS as f64);
                            s.binance_jump_1s * JUMP_PASS_THROUGH * remaining
                        } else {
                            0.0
                        }
                    }
                    None => 0.0,
                },
                Err(_) => 0.0,
            };
            let spot_s = chainlink_base + jump_correction;

            // 步骤 B：FvEngine 一次性给出 σ / FV / iv_poly / FV 上涨判定
            let fv = self.fv_engine.compute(FvInputs {
                binance_mid: spot_s,
                strike,
                expiry_min,
                poly_p: current_poly_p,
                has_poly,
                steady,
                sticky_sigma: sticky_in,
                sigma_min: self.config.trading.volatility_sigma_min,
                sigma_max_poly: self.config.trading.volatility_sigma_max_poly,
                default_sigma: self.config.trading.default_volatility_annual,
                deribit_surface: deribit_surface.clone(),
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
                // JumpChase 信号: 若距 last_jump_ts_ms < JUMP_CONFIRM_MS，认为 fresh。
                let jump_fresh = match s.last_jump_ts_ms {
                    Some(ts) => (now_ms - ts) < JUMP_CONFIRM_MS,
                    None => false,
                };
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
                    jump_dir: s.last_jump_dir,
                    jump_fresh,
                    binance_mid: mid,
                    strike,
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
                    jump_dir: None,
                    jump_fresh: false,
                    binance_mid: mid,
                    strike,
                }
            };

            // 步骤 D：纯函数 decision::build_intents → 0~2 笔 BuyIntent
            let thresholds = Thresholds {
                jump_chase_qty: JUMP_CHASE_QTY,
                min_order_qty: MIN_ORDER_QTY,
                min_expiry_min_for_chase: MIN_EXPIRY_MIN_FOR_CHASE,
                taker_buy_interval_ms: TAKER_BUY_INTERVAL_MS,
                pair_health_max: PAIR_HEALTH_MAX,
            };
            // v0.6: MAX_BUY_PRICE 兜底守门通过 decision 内置 MAX_ENTRY_PRICE_JUMP 0.65 实现
            let _ = MAX_BUY_PRICE;
            let (intents, effects) = decision::build_intents(
                &fv,
                &snap,
                &thresholds,
                now_ms,
                self.last_taker_up_ts_ms,
                self.last_taker_down_ts_ms,
            );

            // 步骤 E：先驱动 OrderClient.tick（dry-run 推进 sim 队列；实盘做 ttl/reprice 撤单），
            // 再 dispatch 本批 BuyIntent。marketable 单同步推 last_taker_*_ts_ms 节流戳（实盘
            // 无 WS fill listener，靠这条同步反馈维持决策节流稳定）。
            // v0.6: DecisionSideEffects 已空，无副作用字段需要写回。
            let _ = effects;
            if let Ok(mut s) = self.state.write() {
                let (up_fill, down_fill) =
                    self.order_client.tick(&mut s, fair_p, fair_down, now_ms);
                if up_fill {
                    self.last_taker_up_ts_ms = now_ms;
                }
                if down_fill {
                    self.last_taker_down_ts_ms = now_ms;
                }
                for intent in &intents {
                    let (ask, token_id) = match intent.side {
                        ChaseSide::Up => (s.poly_best_ask, s.poly_token_id.clone()),
                        ChaseSide::Down => (s.poly_down_best_ask, s.poly_down_token_id.clone()),
                    };
                    let marketable = ask > 0.0 && ask <= intent.target;
                    if marketable {
                        match intent.side {
                            PositionSide::Up => self.last_taker_up_ts_ms = now_ms,
                            PositionSide::Down => self.last_taker_down_ts_ms = now_ms,
                        }
                    }
                    self.order_client
                        .dispatch_buy_intent(intent, ask, &token_id, now_ms, &mut s);
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

            // IOC 在 BookTicker 即时成交，PolyBookUpdate 这里只做 merge + 快照。
            let ts_ms = chrono::Utc::now().timestamp_millis();

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
