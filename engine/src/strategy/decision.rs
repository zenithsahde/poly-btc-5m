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

/// 决策守门与 worst_price 参数；与 signal.rs 的常量同源，集中在一处便于调参
pub struct Thresholds {
    pub chase_gap_min: f64,
    pub safety_margin: f64,
    pub chase_worst_slippage: f64,
    /// JumpChase 腿允许的 worst-target 滑点。比 chase 大（4c vs 2c），因为
    /// 抢 chainlink 跟随 binance 的 5s alpha 时机珍贵，吃稍贵的档可接受。
    pub jump_worst_slippage: f64,
    /// JumpChase 触发的最小 FV-poly_mid 错价（cent）。
    /// 比 chase_gap_min 低（3c vs 10c）：因为 chainlink 已经"快要追上 binance" 是高确信信号，
    /// 不需要等 FV 完全 dominate poly_mid 才下手。
    pub jump_chase_gap_min: f64,
    /// JumpChase 单笔基础张数（小仓抢跑）。
    pub jump_chase_qty: f64,
    pub rebal_worst_head_room: f64,
    pub pair_health_max: f64,
    pub rebal_health_max: f64,
    pub max_buy_price: f64,
    pub chase_min_qty: f64,
    pub min_order_qty: f64,
    pub min_expiry_min_for_chase: f64,
    pub min_expiry_min_for_rebal: f64,
    pub taker_buy_interval_ms: i64,
    pub force_balance_slack: f64,
    pub chase_only_in_excited: bool,
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
    let in_excited = !snap.steady;
    let in_chase_window = snap.expiry_min >= th.min_expiry_min_for_chase;
    let in_rebal_window = snap.expiry_min >= th.min_expiry_min_for_rebal;

    // 末段方向守门（方案 W）：距窗口末 < 1 分钟时，binance 已经远离 strike $30+ → 方向几乎锁定，
    // 此时只允许 chase 赢家方，避免在末段押错方向后残仓 settle 全亏（线上实证 -$26/-$62）。
    // binance 是领先源，poly MM 也是这么看的，跟着这个方向不会错。
    const LOCKIN_WINDOW_MIN: f64 = 1.0;
    const LOCKIN_USD_THRESHOLD: f64 = 30.0;
    let in_lockin = snap.expiry_min < LOCKIN_WINDOW_MIN;
    let binance_vs_k = snap.binance_mid - snap.strike;
    let lockin_blocks_up = in_lockin && binance_vs_k < -LOCKIN_USD_THRESHOLD;
    let lockin_blocks_down = in_lockin && binance_vs_k > LOCKIN_USD_THRESHOLD;

    // v0.6 砍掉 chase（FV gap-based）—— 经实证 fee 吃掉 alpha 还产生残仓 settle 损失。
    // 唯一 alpha 入口 = up_jump / down_jump（纯 binance jump 事件驱动）。
    let up_chase = false;
    let down_chase = false;
    let _ = (fv, in_excited);  // suppress unused
    let chase_side = None;

    // v0.6 Pure Jump Capture: 纯事件触发，无 FV 校验，无 validation 等待。
    //   - 不检查 fair_up - poly_mid（避免 FV 计算滞后）
    //   - 不检查 jump_fresh 800ms（用 5s 全 confirm 窗口）—— snap.jump_fresh 已用 JUMP_CONFIRM_MS=5000
    //   - 加 max_entry_price 0.65（已贵不进）
    //   - 加 max_expiry_min 4.5（太早 jump 信息量不够预测 5min 后方向）
    //   - 末段方向守门保留（W）：jump 末段押错方向 = -EV
    // 设计意图：让"binance 涨了 → 立刻 IOC buy UP"这个动作毫秒不延迟，吃 poly book 反应迟缓的窗口。
    const MAX_ENTRY_PRICE_JUMP: f64 = 0.65;
    const MAX_EXPIRY_MIN_JUMP: f64 = 4.5;
    let jump_window_ok = in_chase_window && snap.expiry_min < MAX_EXPIRY_MIN_JUMP;
    let up_jump = jump_window_ok
        && snap.jump_fresh
        && snap.jump_dir == Some(1)
        && snap.up_ask > 0.0
        && snap.up_ask <= MAX_ENTRY_PRICE_JUMP
        && !lockin_blocks_up;
    let down_jump = jump_window_ok
        && snap.jump_fresh
        && snap.jump_dir == Some(-1)
        && snap.down_ask > 0.0
        && snap.down_ask <= MAX_ENTRY_PRICE_JUMP
        && !lockin_blocks_down;

    // force-balance：只允许买"持仓 ≤ 对侧 + slack"的方向
    let balance_ok_up = snap.qty_up <= snap.qty_down + th.force_balance_slack;
    let balance_ok_down = snap.qty_down <= snap.qty_up + th.force_balance_slack;

    // chase 需要激变 + lead + force-balance + 1s 节流（与 origin/main 一致，不加 poly_fresh 守门）
    let chase_buy_up = up_chase
        && balance_ok_up
        && in_excited
        && now_ms - last_taker_up_ts_ms >= th.taker_buy_interval_ms;
    let chase_buy_down = down_chase
        && balance_ok_down
        && in_excited
        && now_ms - last_taker_down_ts_ms >= th.taker_buy_interval_ms;

    // JumpChase 走与 chase 同一节流（避免与 chase 同一 tick 重复下单）+ force-balance
    let jump_buy_up = up_jump
        && balance_ok_up
        && now_ms - last_taker_up_ts_ms >= th.taker_buy_interval_ms
        && !chase_buy_up;
    let jump_buy_down = down_jump
        && balance_ok_down
        && now_ms - last_taker_down_ts_ms >= th.taker_buy_interval_ms
        && !chase_buy_down;

    // 偏仓配平：稳态也允许，无激变态/节流要求；窗口要求比 chase 宽，用来拯救已建偏仓。
    let imbalanced_up_needed = snap.qty_down > snap.qty_up;
    let imbalanced_dn_needed = snap.qty_up > snap.qty_down;
    // v0.6 砍掉 rebal —— 没 SELL 时 rebal 等于"再买一笔反方向 hold to settle"，
    // 多一笔 fee 还引入残仓风险。jump capture 直接 hold to settle，不配平。
    let _ = (imbalanced_up_needed, imbalanced_dn_needed, in_rebal_window);
    let rebalance_buy_up_path = false;
    let rebalance_buy_down_path = false;

    // 计算配平缺口用于展示和 qty 裁剪
    let proj_up = snap.qty_up;
    let proj_down = snap.qty_down;
    let rebalance_qty_up = (proj_down - proj_up).max(0.0);
    let rebalance_qty_down = (proj_up - proj_down).max(0.0);
    let avg_after_up = weighted_avg(
        snap.qty_up,
        snap.avg_up,
        rebalance_qty_up,
        (snap.up_ask - 0.01).max(0.0),
    ) + snap.avg_down;
    let avg_after_down = snap.avg_up
        + weighted_avg(
            snap.qty_down,
            snap.avg_down,
            rebalance_qty_down,
            (snap.down_ask - 0.01).max(0.0),
        );

    let trigger_up = chase_buy_up || jump_buy_up || rebalance_buy_up_path;
    let trigger_down = chase_buy_down || jump_buy_down || rebalance_buy_down_path;

    // chase 抓 lead alpha → 固定策略量；jump 抢 chainlink 跟随窗口 → 小仓试探；
    // rebalance 配平 → 按真实缺口下单（兜 Polymarket 最低张数）。
    // 优先级：chase > jump > rebalance（同侧并发时 chase 优先，与下方 reason 判定一致）。
    let order_qty_up = if chase_buy_up {
        th.chase_min_qty
    } else if jump_buy_up {
        th.jump_chase_qty
    } else if rebalance_buy_up_path {
        rebalance_qty_up.max(th.min_order_qty)
    } else {
        0.0
    };
    let order_qty_down = if chase_buy_down {
        th.chase_min_qty
    } else if jump_buy_down {
        th.jump_chase_qty
    } else if rebalance_buy_down_path {
        rebalance_qty_down.max(th.min_order_qty)
    } else {
        0.0
    };

    // pair-health 守门：建仓腿宽松 pair_health_max（对侧 ask proxy），配平腿用 rebal_health_max（对侧真实 avg）
    // 方案 Y：末段动态放宽。距窗口末 < 1 min 时，赢家方 ask 飞到 0.9+ 让正常配平守门拒掉，
    // 残仓 settle 全亏。末段放宽到 lockin_rebal_health_max=1.20：哪怕配平时锁住一点亏，
    // 也比残仓 settle 0 redeem 强（数学上残仓 cost > 0 时配平永远更优）。
    const LOCKIN_REBAL_HEALTH_MAX: f64 = 1.20;
    const LOCKIN_PAIR_HEALTH_MAX: f64 = 1.20;
    let effective_rebal_health_max = if in_lockin {
        LOCKIN_REBAL_HEALTH_MAX
    } else {
        th.rebal_health_max
    };
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
    let pair_health_ok_up = if snap.qty_down > 0.0 {
        (new_avg_up_after + snap.avg_down) < effective_rebal_health_max
    } else {
        let opp_proxy = if snap.down_ask > 0.0 {
            snap.down_ask
        } else {
            0.5
        };
        (new_avg_up_after + opp_proxy) < effective_pair_health_max
    };
    let pair_health_ok_down = if snap.qty_up > 0.0 {
        (snap.avg_up + new_avg_down_after) < effective_rebal_health_max
    } else {
        let opp_proxy = if snap.up_ask > 0.0 { snap.up_ask } else { 0.5 };
        (opp_proxy + new_avg_down_after) < effective_pair_health_max
    };

    let target_up = (fv.fair_up - th.safety_margin).max(0.0);
    let target_down = (fv.fair_down - th.safety_margin).max(0.0);
    // v0.6: JumpChase 不用 fv-based target 守门（fv 可能因 chainlink 滞后失真）。
    // jump_buy_up/down 已经自己检查 up_ask <= 0.65。
    // chase/rebal 已 disabled，保留下面这一行守门兼容历史（实际不会触发）。
    let want_buy_up = trigger_up && pair_health_ok_up;
    let want_buy_down = trigger_down && pair_health_ok_down;
    let _ = (target_up, target_down, th.max_buy_price);

    let mut intents: Vec<BuyIntent> = Vec::with_capacity(2);
    if want_buy_up {
        let reason = if chase_buy_up {
            PendingOrderReason::Chase
        } else if jump_buy_up {
            PendingOrderReason::JumpChase
        } else {
            PendingOrderReason::Rebalance
        };
        // worst = IOC 走簿硬上限。
        //   Chase: target + chase_worst_slippage（走簿 2c）
        //   JumpChase v0.6: worst = up_ask 精确（**只吃 best_ask 一档**，不走簿），
        //     避免薄盘 walk 多档把 alpha 吞了。target 也设为 ask（与 worst 一致表示我们就吃这价）。
        //   Rebalance: 吃 merge 分水岭 (1 − avg_down) − head_room
        let (intent_target_up, worst_up) = match reason {
            PendingOrderReason::Chase => (target_up, target_up + th.chase_worst_slippage),
            PendingOrderReason::JumpChase => (snap.up_ask, snap.up_ask),
            PendingOrderReason::Rebalance => {
                let w = ((1.0 - snap.avg_down) - th.rebal_worst_head_room).max(0.0);
                (target_up, w)
            }
        };
        intents.push(BuyIntent {
            side: ChaseSide::Up,
            qty: order_qty_up,
            target: intent_target_up,
            worst: worst_up,
            reason,
        });
    }
    if want_buy_down {
        let reason = if chase_buy_down {
            PendingOrderReason::Chase
        } else if jump_buy_down {
            PendingOrderReason::JumpChase
        } else {
            PendingOrderReason::Rebalance
        };
        let (intent_target_down, worst_down) = match reason {
            PendingOrderReason::Chase => (target_down, target_down + th.chase_worst_slippage),
            PendingOrderReason::JumpChase => (snap.down_ask, snap.down_ask),
            PendingOrderReason::Rebalance => {
                let w = ((1.0 - snap.avg_up) - th.rebal_worst_head_room).max(0.0);
                (target_down, w)
            }
        };
        intents.push(BuyIntent {
            side: ChaseSide::Down,
            qty: order_qty_down,
            target: intent_target_down,
            worst: worst_down,
            reason,
        });
    }

    let effects = DecisionSideEffects {
        chase_side: if snap.steady { None } else { chase_side },
        rebalance_hint_up: if rebalance_buy_up_path && rebalance_qty_up >= th.min_order_qty {
            Some((rebalance_qty_up, avg_after_up))
        } else {
            None
        },
        rebalance_hint_down: if rebalance_buy_down_path && rebalance_qty_down >= th.min_order_qty {
            Some((rebalance_qty_down, avg_after_down))
        } else {
            None
        },
    };
    (intents, effects)
}

fn weighted_avg(old_qty: f64, old_avg: f64, add_qty: f64, add_price: f64) -> f64 {
    let total = old_qty + add_qty;
    if total > 0.0 {
        (old_avg * old_qty + add_price * add_qty) / total
    } else {
        add_price
    }
}
