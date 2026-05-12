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
///   1. poly_fresh    — Poly WS 新鲜，避免基于陈旧簿下单
///   2. in_chase_window — 距窗口结束 ≥ 1.5min（末段不新建 chase 仓）
///   3. force-balance — 只买"持仓 ≤ 对侧"的一边
///   4. min qty       — ≥ 5 张（Polymarket 最低）
///   5. pair-health   — 建仓腿 < 1.05 / 配平腿 < 0.98（均价之和）
///   6. max buy price — target ≤ 0.85（防尾部 49:1 爆损）
///
/// worst_price 映射（IOC walk-the-book 的硬上限）：
///   - chase     : min(target + CHASE_WORST_SLIPPAGE, MAX_BUY_PRICE)
///   - rebalance : min((1 - opp.avg) - REBAL_WORST_HEAD_ROOM, MAX_BUY_PRICE)
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
}

/// 决策守门与 worst_price 参数；与 signal.rs 的常量同源，集中在一处便于调参
pub struct Thresholds {
    pub chase_gap_min: f64,
    pub safety_margin: f64,
    pub chase_worst_slippage: f64,
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
    /// 当前决策帧的展示值（供 TUI 用），不影响执行
    pub rebalance_hint: Option<(f64, f64)>,
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

    // lead 信号：FV 显著高于 Poly 且正在上涨
    let up_chase = th.chase_only_in_excited
        && in_excited
        && in_chase_window
        && snap.poly_up_mid > 0.0
        && (fv.fair_up - snap.poly_up_mid) >= th.chase_gap_min
        && fv.fv_up_rising;
    let down_chase = th.chase_only_in_excited
        && in_excited
        && in_chase_window
        && snap.poly_down_mid > 0.0
        && (fv.fair_down - snap.poly_down_mid) >= th.chase_gap_min
        && fv.fv_down_rising;
    let chase_side = if up_chase {
        Some(ChaseSide::Up)
    } else if down_chase {
        Some(ChaseSide::Down)
    } else {
        None
    };

    // force-balance：只允许买"持仓 ≤ 对侧 + slack"的方向
    let balance_ok_up = snap.qty_up <= snap.qty_down + th.force_balance_slack;
    let balance_ok_down = snap.qty_down <= snap.qty_up + th.force_balance_slack;

    // chase 需要激变 + lead + force-balance + Poly 新鲜 + 1s 节流
    let chase_buy_up = up_chase
        && balance_ok_up
        && in_excited
        && snap.poly_fresh
        && now_ms - last_taker_up_ts_ms >= th.taker_buy_interval_ms;
    let chase_buy_down = down_chase
        && balance_ok_down
        && in_excited
        && snap.poly_fresh
        && now_ms - last_taker_down_ts_ms >= th.taker_buy_interval_ms;

    // 偏仓配平：稳态也允许，无激变态/节流要求；窗口要求比 chase 宽，用来拯救已建偏仓。
    let imbalanced_up_needed = snap.qty_down > snap.qty_up;
    let imbalanced_dn_needed = snap.qty_up > snap.qty_down;
    let rebalance_buy_up_path = imbalanced_up_needed && in_rebal_window && snap.poly_fresh;
    let rebalance_buy_down_path = imbalanced_dn_needed && in_rebal_window && snap.poly_fresh;

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

    let trigger_up = chase_buy_up || rebalance_buy_up_path;
    let trigger_down = chase_buy_down || rebalance_buy_down_path;

    // chase 固定打基础仓位；rebalance 只补真实缺口，避免小偏仓时反向打过头
    let order_qty_up = if chase_buy_up {
        th.chase_min_qty
    } else if rebalance_buy_up_path {
        rebalance_qty_up.min(th.chase_min_qty)
    } else {
        0.0
    };
    let order_qty_down = if chase_buy_down {
        th.chase_min_qty
    } else if rebalance_buy_down_path {
        rebalance_qty_down.min(th.chase_min_qty)
    } else {
        0.0
    };
    let qty_ok_up = order_qty_up >= th.min_order_qty;
    let qty_ok_down = order_qty_down >= th.min_order_qty;

    // pair-health 守门：建仓腿宽松 1.05（对侧 ask proxy），配平腿严格 0.98（对侧真实 avg）
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
        (new_avg_up_after + snap.avg_down) < th.rebal_health_max
    } else {
        let opp_proxy = if snap.down_ask > 0.0 {
            snap.down_ask
        } else {
            0.5
        };
        (new_avg_up_after + opp_proxy) < th.pair_health_max
    };
    let pair_health_ok_down = if snap.qty_up > 0.0 {
        (snap.avg_up + new_avg_down_after) < th.rebal_health_max
    } else {
        let opp_proxy = if snap.up_ask > 0.0 { snap.up_ask } else { 0.5 };
        (opp_proxy + new_avg_down_after) < th.pair_health_max
    };

    let target_up = (fv.fair_up - th.safety_margin).max(0.0);
    let target_down = (fv.fair_down - th.safety_margin).max(0.0);
    let want_buy_up = trigger_up
        && qty_ok_up
        && pair_health_ok_up
        && target_up > 0.0
        && target_up <= th.max_buy_price;
    let want_buy_down = trigger_down
        && qty_ok_down
        && pair_health_ok_down
        && target_down > 0.0
        && target_down <= th.max_buy_price;

    let mut intents: Vec<BuyIntent> = Vec::with_capacity(2);
    if want_buy_up {
        let worst_up = if chase_buy_up {
            (target_up + th.chase_worst_slippage).min(th.max_buy_price)
        } else if snap.avg_down > 0.0 {
            ((1.0 - snap.avg_down) - th.rebal_worst_head_room).min(th.max_buy_price)
        } else {
            (target_up + th.chase_worst_slippage).min(th.max_buy_price)
        };
        let reason = if chase_buy_up {
            PendingOrderReason::Chase
        } else {
            PendingOrderReason::Rebalance
        };
        intents.push(BuyIntent {
            side: ChaseSide::Up,
            qty: order_qty_up,
            target: target_up,
            worst: worst_up,
            reason,
            rebalance_hint: None,
        });
    }
    if want_buy_down {
        let worst_down = if chase_buy_down {
            (target_down + th.chase_worst_slippage).min(th.max_buy_price)
        } else if snap.avg_up > 0.0 {
            ((1.0 - snap.avg_up) - th.rebal_worst_head_room).min(th.max_buy_price)
        } else {
            (target_down + th.chase_worst_slippage).min(th.max_buy_price)
        };
        let reason = if chase_buy_down {
            PendingOrderReason::Chase
        } else {
            PendingOrderReason::Rebalance
        };
        intents.push(BuyIntent {
            side: ChaseSide::Down,
            qty: order_qty_down,
            target: target_down,
            worst: worst_down,
            reason,
            rebalance_hint: None,
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
