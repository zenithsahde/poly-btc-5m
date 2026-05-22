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
                        format!("{} · 剩 {:.1}min", slug, expiry)
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
                <div class="card-title mb-0">"Order Log · 成交日志"</div>
                <div class="flex items-center gap-3">
                    <span class="text-[10px] text-zinc-500 numeric">{move || window_info.get()}</span>
                    <span class="text-[10px] text-zinc-400 numeric">
                        {move || format!("{} 条", event_count.get())}
                    </span>
                </div>
            </div>
            <div class="text-[10px] text-zinc-500 mb-2 leading-relaxed">
                "类型: " <span class="text-emerald-400">"成交"</span> "=钱已落 · "
                <span class="text-amber-400">"挂单"</span> "=等待中 · "
                <span class="text-rose-400">"拒单"</span> "=worst 打不过盘口 · "
                <span class="text-zinc-400">"撤单"</span> "=IOC 剩余取消"
                " | 原因: chase=FV>poly · rebal=偏仓配平 · jump_chase=binance ±$30 抢跑"
            </div>
            <div class="order-log-header">
                <span class="ol-col-time">"时间"</span>
                <span class="ol-col-kind">"类型"</span>
                <span class="ol-col-liq">"流动性"</span>
                <span class="ol-col-side">"方向"</span>
                <span class="ol-col-detail">"故事 (谁触发 → 做了什么 → 结果)"</span>
                <span class="ol-col-qty">"张数"</span>
                <span class="ol-col-price">"价格"</span>
                <span class="ol-col-pnl">"现金流"</span>
            </div>
            <div class="order-log-body">
                {move || {
                    let rows = events.get();
                    if rows.is_empty() {
                        view! {
                            <div class="order-log-empty">"等待订单活动…"</div>
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
    // 中文 kind 走新分类
    let kind_class = match event.kind.as_str() {
        "成交" => "ol-kind ol-kind-fill",
        "挂单" => "ol-kind ol-kind-post",
        "撤单" => "ol-kind ol-kind-cancel",
        "拒单" => "ol-kind ol-kind-reject",
        // 兼容旧英文 kind
        "FILL" | "FILL_TAKER" | "FILL_MAKER" | "MATCH" | "TAKER" | "MAKER" => "ol-kind ol-kind-fill",
        "POST" | "ORDER" => "ol-kind ol-kind-post",
        "CANCEL" => "ol-kind ol-kind-cancel",
        "REJECT" => "ol-kind ol-kind-reject",
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
        "ol-col-pnl text-zinc-600"
    };
    let pnl_text = if event.pnl == 0.0 {
        "—".to_string()
    } else {
        fmt_money(event.pnl)
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
            <span class={pnl_class}>{pnl_text}</span>
        </div>
    }
}
