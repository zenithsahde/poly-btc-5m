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
    let merge_pairs = Signal::derive(move || fmt_qty(snapshot.with(|s| s.as_ref().map(|s| s.position.mergeable_pairs).unwrap_or(0.0))));
    let merged = Signal::derive(move || fmt_qty(snapshot.with(|s| s.as_ref().map(|s| s.position.merged_pairs).unwrap_or(0.0))));
    let merge_pnl = Signal::derive(move || fmt_money(snapshot.with(|s| s.as_ref().map(|s| s.position.merge_pnl).unwrap_or(0.0))));
    let fee = Signal::derive(move || format!("-{:.4}", snapshot.with(|s| s.as_ref().map(|s| s.position.total_fee).unwrap_or(0.0))));
    let rebate = Signal::derive(move || format!("+{:.4}", snapshot.with(|s| s.as_ref().map(|s| s.position.total_rebate).unwrap_or(0.0))));
    let cash_pnl = Signal::derive(move || fmt_money(snapshot.with(|s| s.as_ref().map(|s| s.position.cash_pnl).unwrap_or(0.0))));
    let inv_val = Signal::derive(move || format!("{:.4}", snapshot.with(|s| s.as_ref().map(|s| s.position.inventory_value).unwrap_or(0.0))));
    let net = Signal::derive(move || snapshot.with(|s| s.as_ref().map(|s| s.position.net_pnl).unwrap_or(0.0)));
    let net_text = Signal::derive(move || fmt_money(net.get()));
    let net_class = Signal::derive(move || {
        let v = net.get();
        if v > 0.0 { "text-2xl font-semibold numeric cell-bid" }
        else if v < 0.0 { "text-2xl font-semibold numeric cell-ask" }
        else { "text-2xl font-semibold numeric text-zinc-200" }
    });
    let chase = Signal::derive(move || snapshot.with(|s| s.as_ref().and_then(|s| s.position.chase_side.clone()).unwrap_or_else(|| "—".into())));

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
                <Kv k="mergeable" v=merge_pairs />
                <Kv k="merged" v=merged />
                <Kv k="merge pnl" v=merge_pnl />
                <Kv k="fee" v=fee />
                <Kv k="rebate" v=rebate />
                <Kv k="cash pnl" v=cash_pnl />
                <Kv k="inventory" v=inv_val />
            </div>
            <div class="mt-3 pt-3 border-t border-border flex items-baseline justify-between">
                <span class="text-xs text-zinc-400 uppercase tracking-wider">"net pnl"</span>
                <span class={move || net_class.get()}>{move || net_text.get()}</span>
            </div>
        </div>
    }
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
