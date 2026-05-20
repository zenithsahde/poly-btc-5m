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
    let fee_value = Signal::derive(move || snapshot.with(|s| s.as_ref().map(|s| s.position.total_fee).unwrap_or(0.0)));
    let rebate_value = Signal::derive(move || snapshot.with(|s| s.as_ref().map(|s| s.position.total_rebate).unwrap_or(0.0)));
    let paid_value = Signal::derive(move || snapshot.with(|s| s.as_ref().map(|s| s.position.cash_paid).unwrap_or(0.0)));
    let received_value = Signal::derive(move || snapshot.with(|s| s.as_ref().map(|s| s.position.cash_received).unwrap_or(0.0)));
    let fee = Signal::derive(move || format!("-{:.4}", fee_value.get()));
    let rebate = Signal::derive(move || format!("+{:.4}", rebate_value.get()));
    let paid = Signal::derive(move || format!("-{:.4}", paid_value.get()));
    let received = Signal::derive(move || format!("+{:.4}", received_value.get()));
    let cash_pnl = Signal::derive(move || fmt_money(snapshot.with(|s| s.as_ref().map(|s| s.position.cash_pnl).unwrap_or(0.0))));
    let inv_val = Signal::derive(move || format!("{:.4}", snapshot.with(|s| s.as_ref().map(|s| s.position.inventory_value).unwrap_or(0.0))));
    let capital_note = Signal::derive(move || {
        format!(
            "recv {} + inv {} - paid {} - fee {} + rebate {}",
            received.get(),
            inv_val.get(),
            paid.get(),
            fee.get(),
            rebate.get()
        )
    });
    let net = Signal::derive(move || snapshot.with(|s| s.as_ref().map(|s| s.position.net_pnl).unwrap_or(0.0)));
    let net_text = Signal::derive(move || fmt_money(net.get()));
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
    let last_ioc = Signal::derive(move || snapshot.with(|s| {
        s.as_ref()
            .and_then(|s| s.position.ioc_last_order.clone())
            .unwrap_or_else(|| "—".to_string())
    }));

    view! {
        <div class="card position-card">
            <div class="flex items-center justify-between mb-3">
                <div class="card-title mb-0">"Position · PnL"</div>
                <div class="text-xs text-zinc-500 numeric">
                    "chase " <span class="text-zinc-300">{move || chase.get()}</span>
                </div>
            </div>

            <div class="position-hero">
                <MoneyTile label="net pnl" value=net_text note=capital_note emph=true value_class=Signal::derive(move || net_value_class(net.get())) />
                <MoneyTile label="paid" value=paid note=Signal::derive(move || "total buy cost".to_string()) emph=false value_class=Signal::derive(move || "money-value cell-ask".to_string()) />
                <MoneyTile label="received" value=received note=Signal::derive(move || "merge / redeem cash".to_string()) emph=false value_class=Signal::derive(move || "money-value cell-bid".to_string()) />
                <MoneyTile label="inventory" value=inv_val note=Signal::derive(move || "pairs at $1 + residual mid".to_string()) emph=false value_class=Signal::derive(move || "money-value".to_string()) />
                <MoneyTile label="cash pnl" value=cash_pnl note=Signal::derive(move || "received - paid".to_string()) emph=false value_class=Signal::derive(move || money_value_class(snapshot.with(|s| s.as_ref().map(|s| s.position.cash_pnl).unwrap_or(0.0)))) />
                <MoneyTile label="taker fee" value=fee note=Signal::derive(move || "Polymarket fee model".to_string()) emph=false value_class=Signal::derive(move || "money-value cell-ask".to_string()) />
                <MoneyTile label="maker rebate" value=rebate note=Signal::derive(move || "25% fee rebate".to_string()) emph=false value_class=Signal::derive(move || "money-value cell-bid".to_string()) />
                <MoneyTile label="merge pnl" value=merge_pnl note=Signal::derive(move || "sum pairs × (1 - avg_sum)".to_string()) emph=false value_class=Signal::derive(move || money_value_class(snapshot.with(|s| s.as_ref().map(|s| s.position.merge_pnl).unwrap_or(0.0)))) />
            </div>

            <div class="position-section-grid">
                <div class="position-subpanel">
                    <div class="position-subtitle">"Holdings"</div>
                    <SidePos title="UP" qty=up_qty avg=up_avg pnl=up_pnl buy=true />
                    <SidePos title="DOWN" qty=down_qty avg=down_avg pnl=down_pnl buy=false />
                    <Kv k="avg sum" v=avg_sum />
                    <Kv k="pending avg" v=projected />
                    <Kv k="skew" v=skew />
                </div>
                <div class="position-subpanel">
                    <div class="position-subtitle">"Pending / Merge"</div>
                    <Kv k="pending UP" v=pending_up />
                    <Kv k="pending DOWN" v=pending_down />
                    <Kv k="mergeable" v=merge_pairs />
                    <Kv k="merged" v=merged />
                    <Kv k="rebal UP" v=rebalance_up />
                    <Kv k="rebal DOWN" v=rebalance_down />
                </div>
                <div class="position-subpanel">
                    <div class="position-subtitle">"This Window / Fill"</div>
                    <Kv k="win UP" v=window_up />
                    <Kv k="win DOWN" v=window_down />
                    <Kv k="win cost" v=window_cost />
                    <Kv k="fill rows" v=window_rows />
                    <Kv k="orders" v=ioc_orders />
                    <Kv k="fill" v=ioc_fill />
                    <Kv k="worst" v=ioc_worst />
                    <Kv k="partial" v=ioc_partial />
                    <Kv k="levels" v=ioc_levels />
                    <Kv k="chase" v=ioc_chase />
                    <Kv k="rebal" v=ioc_rebal />
                    <Kv k="last" v=last_ioc />
                </div>
            </div>
        </div>
    }
}

fn money_value_class(value: f64) -> String {
    if value > 0.0 {
        "money-value cell-bid".to_string()
    } else if value < 0.0 {
        "money-value cell-ask".to_string()
    } else {
        "money-value".to_string()
    }
}

fn net_value_class(value: f64) -> String {
    if value > 0.0 {
        "money-value cell-bid".to_string()
    } else if value < 0.0 {
        "money-value cell-ask".to_string()
    } else {
        "money-value text-zinc-200".to_string()
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

#[component]
fn MoneyTile(
    label: &'static str,
    value: Signal<String>,
    note: Signal<String>,
    emph: bool,
    value_class: Signal<String>,
) -> impl IntoView {
    let tile_class = if emph { "money-tile money-tile-net" } else { "money-tile" };
    view! {
        <div class={tile_class}>
            <div class="money-label">{label}</div>
            <div class={move || value_class.get()}>{move || value.get()}</div>
            <div class="money-note">{move || note.get()}</div>
        </div>
    }
}
