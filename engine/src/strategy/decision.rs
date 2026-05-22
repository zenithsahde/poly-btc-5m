/// strategy/decision.rs - 建仓决策（纯函数）
///
/// 单一职责：给定一帧 FV 结果 + Poly 盘口快照 + 当前持仓 → 输出 0~2 笔 `BuyIntent`。
/// 不读 AppState、不改任何状态；所有状态副作用留给调用方。
///
/// 两条触发路径：
///   A) chase：吃 FV 领先 Poly 的 1.1–2.5s alpha（激变态 + lead + 节流 + force-balance）
///   B) rebalance：偏仓配平锁 merge 套利（稳态也允许，无节流）
///
/// 守门链（所有必须过）：
///   1. in_chase_window — 距窗口结束 ≥ 1.5min（末段不新建 chase 仓）
///   2. force-balance — chase 只买"持仓 ≤ 对侧"的一边
///   3. pair-health   — 建仓腿 < 1.05 / 配平腿 < 0.98（均价之和）
///   4. max buy price — target ≤ 0.85（防尾部 49:1 爆损）
///
/// 策略层给每笔 intent 算 target（理想价）与 worst（IOC 走簿硬上限）：
///   - chase 腿：worst = target + chase_worst_slippage（容许走簿稍吃贵以抓 alpha）
///   - 配平腿：worst = (1 − 对侧持仓均价) − rebal_worst_head_room
///     —— 钉在 merge 分水岭，确保 fill 后 avg_sum < REBAL_HEALTH_MAX，每对 merge 锁利。
use crate::position::PendingOrderReason;
use crate::strategy::fv::FvResult;
use crate::tui::app::ChaseSide;

/// 参与决策的 Poly 盘口 + ledger 快照。由调用方在一次 read lock 里装填，释放后再传进来。
pub struct MarketSnapshot {
    pub poly_up_mid: f64,
    pub poly_down_mid: f64,
    pub up_ask: f64,
    pub down_ask: f64,
    pub qty_up: f64,
    pub qty_down: f64,
    pub avg_up: f64,
    pub avg_down: f64,
    pub steady: bool,
    pub expiry_min: f64,
    pub poly_fresh: bool,
    /// JumpChase 触发器输入：上次 binance ±$30/1s jump 的方向（+1 上跳 / -1 下跳），
    /// 若距今 > jump_confirm_ms 则视为过期由调用方传 None。
    pub jump_dir: Option<i8>,
    /// jump 信号是否仍在 confirm 窗口内（fresh = true 才允许 JumpChase 触发）。
    pub jump_fresh: bool,
    /// 末段方向守门用：binance mid 现价（领先信号），用于 chase 时判定 binance vs strike 方向
    pub binance_mid: f64,
    /// 末段方向守门用：本窗口 strike (chainlink @ window_start)
    pub strike: f64,
}

/// 决策守门与 worst_price 参数（v0.6 瘦身：只留 JumpChase 路径用的字段）
pub struct Thresholds {
    /// JumpChase 单笔基础张数。
    pub jump_chase_qty: f64,
    /// Polymarket 最低订单数量（5 张）。用于 ioc 内部检查。
    pub min_order_qty: f64,
    /// jump_window 下界（min）：剩余 < 此值禁 jump，避免末段建仓没时间消化。
    pub min_expiry_min_for_chase: f64,
    /// 同侧 taker buy 节流间隔（ms）：避免同一 jump 信号触发多次。
    pub taker_buy_interval_ms: i64,
    /// pair_health 守门：建仓腿对侧空仓时，fill 后 (new_avg + opp_ask) 上限。
    /// 末段动态放宽到 1.20（见 build_intents 内 LOCKIN_PAIR_HEALTH_MAX）。
    pub pair_health_max: f64,
}

#[derive(Debug, Clone)]
pub struct BuyIntent {
    pub side: ChaseSide,
    pub qty: f64,
    pub target: f64,
    pub worst: f64,
    pub reason: PendingOrderReason,
}

/// 面向 signal.rs 的辅助输出：决策过程中得到、但 signal.rs 还要写回 state 的展示字段
pub struct DecisionSideEffects {
    pub chase_side: Option<ChaseSide>,
    pub rebalance_hint_up: Option<(f64, f64)>,
    pub rebalance_hint_down: Option<(f64, f64)>,
}

/// 核心决策函数：纯函数，无副作用
pub fn build_intents(
    fv: &FvResult,
    snap: &MarketSnapshot,
    th: &Thresholds,
    now_ms: i64,
    last_taker_up_ts_ms: i64,
    last_taker_down_ts_ms: i64,
) -> (Vec<BuyIntent>, DecisionSideEffects) {
    // 末段方向守门（方案 W）：距窗口末 < 1 分钟时，binance 已经远离 strike $30+ → 方向几乎锁定。
    // 此时只允许 jump 赢家方，避免在末段押错方向后残仓 settle 全亏。
    const LOCKIN_WINDOW_MIN: f64 = 1.0;
    const LOCKIN_USD_THRESHOLD: f64 = 30.0;
    let in_lockin = snap.expiry_min < LOCKIN_WINDOW_MIN;
    let binance_vs_k = snap.binance_mid - snap.strike;
    let lockin_blocks_up = in_lockin && binance_vs_k < -LOCKIN_USD_THRESHOLD;
    let lockin_blocks_down = in_lockin && binance_vs_k > LOCKIN_USD_THRESHOLD;

    // v0.6 Pure Jump Capture: 纯事件触发，唯一 alpha 入口。
    //   - 不检查 FV gap（避免 FV 计算滞后）
    //   - snap.jump_fresh 已用 JUMP_CONFIRM_MS=5000ms 窗口
    //   - max_entry_price 0.65（已贵不进，asymmetric payoff）
    //   - 1.5 ≤ expiry < 4.5 min（太早信号弱，太晚配平不及）
    //   - lockin 守门避免末段押错方向
    const MAX_ENTRY_PRICE_JUMP: f64 = 0.65;
    const MAX_EXPIRY_MIN_JUMP: f64 = 4.5;
    let in_jump_window =
        snap.expiry_min >= th.min_expiry_min_for_chase && snap.expiry_min < MAX_EXPIRY_MIN_JUMP;
    let up_jump = in_jump_window
        && snap.jump_fresh
        && snap.jump_dir == Some(1)
        && snap.up_ask > 0.0
        && snap.up_ask <= MAX_ENTRY_PRICE_JUMP
        && !lockin_blocks_up;
    let down_jump = in_jump_window
        && snap.jump_fresh
        && snap.jump_dir == Some(-1)
        && snap.down_ask > 0.0
        && snap.down_ask <= MAX_ENTRY_PRICE_JUMP
        && !lockin_blocks_down;

    // 节流：同侧 1s 一笔，避免同 jump 连续下多次
    let jump_buy_up = up_jump && now_ms - last_taker_up_ts_ms >= th.taker_buy_interval_ms;
    let jump_buy_down = down_jump && now_ms - last_taker_down_ts_ms >= th.taker_buy_interval_ms;

    // _fv 仅留作签名兼容（jump 路径不查 FV gap）
    let _ = fv;

    let trigger_up = jump_buy_up;
    let trigger_down = jump_buy_down;

    let order_qty_up = if jump_buy_up { th.jump_chase_qty } else { 0.0 };
    let order_qty_down = if jump_buy_down { th.jump_chase_qty } else { 0.0 };

    // pair-health 守门：fill 后 avg_sum 上限。
    // 静态时 1.02（保证 merge 至少锁 −2c 净亏即可接受，因为 jump 后大概率还会反向 jump 配对）；
    // 末段（< 1 min）放宽到 1.20，让赢家方贵的时候也能配上、把残仓拉回 merge 配对路径。
    const LOCKIN_PAIR_HEALTH_MAX: f64 = 1.20;
    let effective_pair_health_max = if in_lockin {
        LOCKIN_PAIR_HEALTH_MAX
    } else {
        th.pair_health_max
    };
    let new_avg_up_after = if snap.qty_up + order_qty_up > 0.0 {
        (snap.qty_up * snap.avg_up + snap.up_ask * order_qty_up) / (snap.qty_up + order_qty_up)
    } else {
        snap.up_ask
    };
    let new_avg_down_after = if snap.qty_down + order_qty_down > 0.0 {
        (snap.qty_down * snap.avg_down + snap.down_ask * order_qty_down)
            / (snap.qty_down + order_qty_down)
    } else {
        snap.down_ask
    };
    // 对侧若已有持仓用真实 avg；空仓用 ask 作 proxy。两路径都用同一 effective_pair_health_max。
    let pair_health_ok_up = if snap.qty_down > 0.0 {
        (new_avg_up_after + snap.avg_down) < effective_pair_health_max
    } else {
        let opp_proxy = if snap.down_ask > 0.0 {
            snap.down_ask
        } else {
            0.5
        };
        (new_avg_up_after + opp_proxy) < effective_pair_health_max
    };
    let pair_health_ok_down = if snap.qty_up > 0.0 {
        (snap.avg_up + new_avg_down_after) < effective_pair_health_max
    } else {
        let opp_proxy = if snap.up_ask > 0.0 { snap.up_ask } else { 0.5 };
        (opp_proxy + new_avg_down_after) < effective_pair_health_max
    };

    // v0.6: 唯一路径 JumpChase。jump_buy_up/down 已经自己检查 up_ask <= 0.65。
    let want_buy_up = trigger_up && pair_health_ok_up;
    let want_buy_down = trigger_down && pair_health_ok_down;

    let mut intents: Vec<BuyIntent> = Vec::with_capacity(2);
    if want_buy_up {
        // JumpChase: worst = up_ask 精确（只吃 best_ask 一档，不走簿）
        intents.push(BuyIntent {
            side: ChaseSide::Up,
            qty: order_qty_up,
            target: snap.up_ask,
            worst: snap.up_ask,
            reason: PendingOrderReason::JumpChase,
        });
    }
    if want_buy_down {
        intents.push(BuyIntent {
            side: ChaseSide::Down,
            qty: order_qty_down,
            target: snap.down_ask,
            worst: snap.down_ask,
            reason: PendingOrderReason::JumpChase,
        });
    }

    // v0.6: DecisionSideEffects 全部 None（chase_side / rebalance_hint 已废弃但
    // 字段仍在以兼容 AppState/TUI 显示；保留 None 写入即可）
    let effects = DecisionSideEffects {
        chase_side: None,
        rebalance_hint_up: None,
        rebalance_hint_down: None,
    };
    (intents, effects)
}
