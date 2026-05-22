use leptos::prelude::*;

use crate::app::SnapshotSignal;
use crate::format::{fmt_money, fmt_price, fmt_qty};

#[component]
pub fn PositionPanel() -> impl IntoView {
    let SnapshotSignal(snapshot) = use_context().expect("SnapshotSignal");

    let up_qty = Signal::derive(move || snapshot.with(|s| s.as_ref().map(|s| s.position.up.qty).unwrap_or(0.0)));
    let up_avg = Signal::derive(move || snapshot.with(|s| s.as_ref().map(|s| s.position.up.avg_price).unwrap_or(0.0)));
    let up_pnl = Signal::derive(move || snapshot.with(|s| s.as_ref().map(|s| s.position.up.float_pnl).unwrap_or(0.0)));
    let down_qty = Signal::derive(move || snapshot.with(|s| s.as_ref().map(|s| s.position.down.qty).unwrap_or(0.0)));
    let down_avg = Signal::derive(move || snapshot.with(|s| s.as_ref().map(|s| s.position.down.avg_price).unwrap_or(0.0)));
    let down_pnl = Signal::derive(move || snapshot.with(|s| s.as_ref().map(|s| s.position.down.float_pnl).unwrap_or(0.0)));

    let avg_sum = Signal::derive(move || format!("{:.4}", snapshot.with(|s| s.as_ref().map(|s| s.position.avg_sum).unwrap_or(0.0))));
    let projected = Signal::derive(move || snapshot.with(|s| {
        s.as_ref()
            .map(|s| {
                s.position
                    .projected_avg_sum
                    .map(|v| format!("{:.4}", v))
                    .unwrap_or_else(|| "—".to_string())
            })
            .unwrap_or_else(|| "—".to_string())
    }));
    let skew = Signal::derive(move || snapshot.with(|s| {
        s.as_ref()
            .map(|s| s.position.projected_skew.clone())
            .unwrap_or_else(|| "—".to_string())
    }));
    let merge_pairs = Signal::derive(move || fmt_qty(snapshot.with(|s| s.as_ref().map(|s| s.position.mergeable_pairs).unwrap_or(0.0))));
    let merged = Signal::derive(move || snapshot.with(|s| {
        s.as_ref()
            .map(|s| format!("{} ({}x)", fmt_qty(s.position.merged_pairs), s.position.merge_count))
            .unwrap_or_else(|| "—".to_string())
    }));
    let merge_pnl = Signal::derive(move || fmt_money(snapshot.with(|s| s.as_ref().map(|s| s.position.merge_pnl).unwrap_or(0.0))));
    let fee = Signal::derive(move || format!("-{:.4}", snapshot.with(|s| s.as_ref().map(|s| s.position.total_fee).unwrap_or(0.0))));
    let rebate = Signal::derive(move || format!("+{:.4}", snapshot.with(|s| s.as_ref().map(|s| s.position.total_rebate).unwrap_or(0.0))));
    let cash_pnl = Signal::derive(move || fmt_money(snapshot.with(|s| s.as_ref().map(|s| s.position.cash_pnl).unwrap_or(0.0))));
    let inv_val = Signal::derive(move || format!("{:.4}", snapshot.with(|s| s.as_ref().map(|s| s.position.inventory_value).unwrap_or(0.0))));
    let cash_flow = Signal::derive(move || snapshot.with(|s| {
        s.as_ref()
            .map(|s| format!("paid -{:.2} / recv +{:.2}", s.position.cash_paid, s.position.cash_received))
            .unwrap_or_else(|| "—".to_string())
    }));
    let net = Signal::derive(move || snapshot.with(|s| s.as_ref().map(|s| s.position.net_pnl).unwrap_or(0.0)));
    let net_text = Signal::derive(move || fmt_money(net.get()));
    let net_class = Signal::derive(move || {
        let v = net.get();
        if v > 0.0 { "text-2xl font-semibold numeric cell-bid" }
        else if v < 0.0 { "text-2xl font-semibold numeric cell-ask" }
        else { "text-2xl font-semibold numeric text-zinc-200" }
    });
    let chase = Signal::derive(move || snapshot.with(|s| s.as_ref().and_then(|s| s.position.chase_side.clone()).unwrap_or_else(|| "—".into())));
    let pending_up = Signal::derive(move || snapshot.with(|s| {
        s.as_ref()
            .and_then(|s| s.position.maker_buy_intent_up)
            .map(format_pending)
            .unwrap_or_else(|| "—".to_string())
    }));
    let pending_down = Signal::derive(move || snapshot.with(|s| {
        s.as_ref()
            .and_then(|s| s.position.maker_buy_intent_down)
            .map(format_pending)
            .unwrap_or_else(|| "—".to_string())
    }));
    let window_up = Signal::derive(move || snapshot.with(|s| {
        s.as_ref()
            .map(|s| fill_side_summary(s.position.window_up_qty, s.position.window_up_vwap))
            .unwrap_or_else(|| "—".to_string())
    }));
    let window_down = Signal::derive(move || snapshot.with(|s| {
        s.as_ref()
            .map(|s| fill_side_summary(s.position.window_down_qty, s.position.window_down_vwap))
            .unwrap_or_else(|| "—".to_string())
    }));
    let window_cost = Signal::derive(move || snapshot.with(|s| {
        s.as_ref()
            .map(|s| format!("{:.2}", s.position.window_notional))
            .unwrap_or_else(|| "—".to_string())
    }));
    let window_rows = Signal::derive(move || {
        snapshot.with(|s| s.as_ref().map(|s| s.position.window_fill_rows).unwrap_or(0)).to_string()
    });
    let rebalance_up = Signal::derive(move || snapshot.with(|s| {
        s.as_ref()
            .and_then(|s| s.position.rebalance_hint_up)
            .map(format_rebalance)
            .unwrap_or_else(|| "—".to_string())
    }));
    let rebalance_down = Signal::derive(move || snapshot.with(|s| {
        s.as_ref()
            .and_then(|s| s.position.rebalance_hint_down)
            .map(format_rebalance)
            .unwrap_or_else(|| "—".to_string())
    }));
    let ioc_orders = Signal::derive(move || {
        snapshot.with(|s| s.as_ref().map(|s| s.position.ioc_orders).unwrap_or(0)).to_string()
    });
    let ioc_fill = Signal::derive(move || {
        pct(snapshot.with(|s| s.as_ref().map(|s| s.position.ioc_fill_rate).unwrap_or(0.0)))
    });
    let ioc_worst = Signal::derive(move || {
        pct(snapshot.with(|s| s.as_ref().map(|s| s.position.ioc_worst_breach_rate).unwrap_or(0.0)))
    });
    let ioc_partial = Signal::derive(move || {
        pct(snapshot.with(|s| s.as_ref().map(|s| s.position.ioc_partial_rate).unwrap_or(0.0)))
    });
    let ioc_levels = Signal::derive(move || snapshot.with(|s| {
        s.as_ref()
            .map(|s| {
                if s.position.ioc_orders == 0 {
                    "—".to_string()
                } else {
                    format!("{:.1}/{}", s.position.ioc_avg_fill_levels, s.position.ioc_max_fill_levels)
                }
            })
            .unwrap_or_else(|| "—".to_string())
    }));
    let ioc_chase = Signal::derive(move || snapshot.with(|s| {
        s.as_ref()
            .map(|s| format!("{}/{} {}", s.position.ioc_chase_filled, s.position.ioc_chase_orders, pct(s.position.ioc_chase_fill_rate)))
            .unwrap_or_else(|| "0 —".to_string())
    }));
    let ioc_rebal = Signal::derive(move || snapshot.with(|s| {
        s.as_ref()
            .map(|s| format!("{}/{} {}", s.position.ioc_rebal_filled, s.position.ioc_rebal_orders, pct(s.position.ioc_rebal_fill_rate)))
            .unwrap_or_else(|| "0 —".to_string())
    }));
    let ioc_jump = Signal::derive(move || snapshot.with(|s| {
        s.as_ref()
            .map(|s| format!("{}/{} {}", s.position.ioc_jump_chase_filled, s.position.ioc_jump_chase_orders, pct(s.position.ioc_jump_chase_fill_rate)))
            .unwrap_or_else(|| "0 —".to_string())
    }));
    let last_ioc = Signal::derive(move || snapshot.with(|s| {
        s.as_ref()
            .and_then(|s| s.position.ioc_last_order.clone())
            .unwrap_or_else(|| "—".to_string())
    }));

    view! {
        <div class="card">
            <div class="flex items-center justify-between mb-3">
                <div class="card-title mb-0">"Position · PnL"</div>
                <div class="text-xs text-zinc-500 numeric">
                    "chase " <span class="text-zinc-300">{move || chase.get()}</span>
                </div>
            </div>
            <div class="grid grid-cols-2 gap-3 text-xs">
                <SidePos title="UP" qty=up_qty avg=up_avg pnl=up_pnl buy=true />
                <SidePos title="DOWN" qty=down_qty avg=down_avg pnl=down_pnl buy=false />
            </div>
            <div class="mt-3 pt-3 border-t border-border space-y-1">
                <Kv k="avg sum" v=avg_sum />
                <Kv k="pending avg" v=projected />
                <Kv k="skew" v=skew />
                <Kv k="pending UP" v=pending_up />
                <Kv k="pending DOWN" v=pending_down />
                <Kv k="mergeable" v=merge_pairs />
                <Kv k="merged" v=merged />
                <Kv k="merge pnl" v=merge_pnl />
            </div>
            <div class="mt-3 pt-3 border-t border-border space-y-1">
                <Kv k="cash pnl" v=cash_pnl />
                <Kv k="cash flow" v=cash_flow />
                <Kv k="inventory" v=inv_val />
                <Kv k="fee" v=fee />
                <Kv k="rebate" v=rebate />
            </div>
            <div class="mt-3 pt-3 border-t border-border space-y-1">
                <Kv k="win UP" v=window_up />
                <Kv k="win DOWN" v=window_down />
                <Kv k="win cost" v=window_cost />
                <Kv k="fill rows" v=window_rows />
                <Kv k="rebal UP" v=rebalance_up />
                <Kv k="rebal DOWN" v=rebalance_down />
            </div>
            <div class="mt-3 pt-3 border-t border-border space-y-1">
                <Kv k="ioc orders" v=ioc_orders />
                <Kv k="ioc fill" v=ioc_fill />
                <Kv k="worst" v=ioc_worst />
                <Kv k="partial" v=ioc_partial />
                <Kv k="levels" v=ioc_levels />
                <Kv k="chase" v=ioc_chase />
                <Kv k="rebal" v=ioc_rebal />
                <Kv k="jump" v=ioc_jump />
                <Kv k="last ioc" v=last_ioc />
            </div>
            <div class="mt-3 pt-3 border-t border-border flex items-baseline justify-between">
                <span class="text-xs text-zinc-400 uppercase tracking-wider">"net pnl"</span>
                <span class={move || net_class.get()}>{move || net_text.get()}</span>
            </div>
        </div>
    }
}

fn pct(value: f64) -> String {
    if value <= 0.0 {
        "—".to_string()
    } else {
        format!("{:.0}%", value * 100.0)
    }
}

fn format_pending((price, qty, _ts): (f64, f64, i64)) -> String {
    format!("买@{:.2}×{:.0}", price, qty)
}

fn fill_side_summary(qty: f64, vwap: f64) -> String {
    if qty > 0.0 {
        format!("{:.0}@{:.2}", qty, vwap)
    } else {
        "0".to_string()
    }
}

fn format_rebalance((qty, avg): (f64, f64)) -> String {
    format!("需{:.0}张 avg_sum={:.2}", qty, avg)
}

#[component]
fn SidePos(
    title: &'static str,
    qty: Signal<f64>,
    avg: Signal<f64>,
    pnl: Signal<f64>,
    buy: bool,
) -> impl IntoView {
    let header_cls = if buy {
        "text-[10px] uppercase tracking-wider font-semibold cell-bid"
    } else {
        "text-[10px] uppercase tracking-wider font-semibold cell-ask"
    };
    let pnl_class = Signal::derive(move || {
        let v = pnl.get();
        if v > 0.0 { "kv-val cell-bid" }
        else if v < 0.0 { "kv-val cell-ask" }
        else { "kv-val" }
    });
    view! {
        <div class="space-y-1">
            <div class={header_cls}>{title}</div>
            <div class="kv"><span class="kv-key">"qty"</span><span class="kv-val">{move || fmt_qty(qty.get())}</span></div>
            <div class="kv"><span class="kv-key">"avg"</span><span class="kv-val">{move || fmt_price(avg.get())}</span></div>
            <div class="kv">
                <span class="kv-key">"pnl"</span>
                <span class={move || pnl_class.get()}>{move || fmt_money(pnl.get())}</span>
            </div>
        </div>
    }
}

#[component]
fn Kv(k: &'static str, v: Signal<String>) -> impl IntoView {
    view! {
        <div class="kv">
            <span class="kv-key">{k}</span>
            <span class="kv-val">{move || v.get()}</span>
        </div>
    }
}
