use leptos::prelude::*;
use shared_types::OrderEventView;

use crate::app::SnapshotSignal;
use crate::format::{fmt_money, fmt_ts_ms};

#[component]
pub fn OrderLogPanel() -> impl IntoView {
    let SnapshotSignal(snapshot) = use_context().expect("SnapshotSignal");

    let events = Signal::derive(move || {
        snapshot.with(|s| {
            s.as_ref()
                .map(|s| s.position.order_events.clone())
                .unwrap_or_default()
        })
    });

    let window_info = Signal::derive(move || {
        snapshot.with(|s| {
            s.as_ref()
                .map(|s| {
                    let slug = &s.poly.slug;
                    let end_ts = s.poly.window_end_ts;
                    let expiry = s.poly.expiry_minutes;
                    if end_ts > 0 {
                        format!("{} · {:.1}m left", slug, expiry)
                    } else {
                        slug.clone()
                    }
                })
                .unwrap_or_default()
        })
    });

    let event_count = Signal::derive(move || events.with(|e| e.len()));

    view! {
        <div class="card order-log-card">
            <div class="flex items-center justify-between mb-3">
                <div class="card-title mb-0">"Order Log"</div>
                <div class="flex items-center gap-3">
                    <span class="text-[10px] text-zinc-500 numeric">{move || window_info.get()}</span>
                    <span class="text-[10px] text-zinc-400 numeric">
                        {move || format!("{} events", event_count.get())}
                    </span>
                </div>
            </div>
            <div class="order-log-header">
                <span class="ol-col-time">"Time"</span>
                <span class="ol-col-kind">"Type"</span>
                <span class="ol-col-liq">"Liq"</span>
                <span class="ol-col-side">"Side"</span>
                <span class="ol-col-detail">"Detail"</span>
                <span class="ol-col-qty">"Qty"</span>
                <span class="ol-col-price">"Price"</span>
                <span class="ol-col-pnl">"PnL"</span>
            </div>
            <div class="order-log-body">
                {move || {
                    let rows = events.get();
                    if rows.is_empty() {
                        view! {
                            <div class="order-log-empty">"waiting for order activity…"</div>
                        }.into_any()
                    } else {
                        rows.into_iter()
                            .map(|row| view! { <OrderLogRow event=row /> })
                            .collect_view()
                            .into_any()
                    }
                }}
            </div>
        </div>
    }
}

#[component]
fn OrderLogRow(event: OrderEventView) -> impl IntoView {
    let kind_class = match event.kind.as_str() {
        "FILL" | "FILL_TAKER" | "FILL_MAKER" | "MATCH" => "ol-kind ol-kind-fill",
        "POST" | "ORDER" => "ol-kind ol-kind-post",
        "CANCEL" => "ol-kind ol-kind-cancel",
        "REJECT" => "ol-kind ol-kind-reject",
        "TAKER" => "ol-kind ol-kind-fill",
        "MAKER" => "ol-kind ol-kind-fill",
        _ => "ol-kind",
    };
    let side_class = match event.side.as_str() {
        "UP" => "ol-side ol-side-up",
        "DOWN" => "ol-side ol-side-down",
        _ => "ol-side",
    };
    let pnl_class = if event.pnl > 0.0 {
        "ol-col-pnl cell-bid"
    } else if event.pnl < 0.0 {
        "ol-col-pnl cell-ask"
    } else {
        "ol-col-pnl"
    };

    view! {
        <div class="order-log-row">
            <span class="ol-col-time">{fmt_ts_ms(event.ts_ms)}</span>
            <span class={kind_class}>{event.kind}</span>
            <span class="ol-col-liq">{event.liquidity}</span>
            <span class={side_class}>{event.side}</span>
            <span class="ol-col-detail">{event.summary}</span>
            <span class="ol-col-qty">{format!("{:.0}", event.qty)}</span>
            <span class="ol-col-price">{format!("{:.2}", event.price)}</span>
            <span class={pnl_class}>{fmt_money(event.pnl)}</span>
        </div>
    }
}
