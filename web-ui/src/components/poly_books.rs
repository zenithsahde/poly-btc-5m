use leptos::prelude::*;
use shared_types::{Level, PolyBook};

use crate::app::SnapshotSignal;
use crate::format::{fmt_price, fmt_qty};

#[component]
pub fn PolymarketPanel() -> impl IntoView {
    let SnapshotSignal(snapshot) = use_context().expect("SnapshotSignal");

    let slug = Signal::derive(move || {
        snapshot.with(|s| s.as_ref().map(|s| s.poly.slug.clone()).unwrap_or_default())
    });
    let expiry = Signal::derive(move || {
        snapshot.with(|s| {
            s.as_ref()
                .map(|s| format!("{:.2}", s.poly.expiry_minutes))
                .unwrap_or_else(|| "—".into())
        })
    });
    let delay_text = Signal::derive(move || {
        snapshot.with(|s| {
            s.as_ref()
                .and_then(|s| s.poly.poly_delay_ms)
                .map(|d| format!("{}ms", d))
                .unwrap_or_default()
        })
    });
    let has_delay =
        move || snapshot.with(|s| s.as_ref().and_then(|s| s.poly.poly_delay_ms).is_some());

    let up = Signal::derive(move || {
        snapshot.with(|s| s.as_ref().map(|s| s.poly.up.clone()).unwrap_or_default())
    });
    let down = Signal::derive(move || {
        snapshot.with(|s| s.as_ref().map(|s| s.poly.down.clone()).unwrap_or_default())
    });

    view! {
        <div class="card">
            <div class="flex items-center justify-between mb-3">
                <div class="card-title mb-0">"Polymarket · 5m UP / DOWN"</div>
                <div class="text-xs text-zinc-500 numeric">
                    "expiry " <span class="text-zinc-300">{move || expiry.get()}</span> " min"
                    <Show when={move || has_delay()}>
                        " · delay " <span class="text-zinc-300">{move || delay_text.get()}</span>
                    </Show>
                </div>
            </div>
            <div class="text-[11px] text-zinc-500 truncate mb-3">{move || slug.get()}</div>
            <div class="grid grid-cols-2 gap-3 text-xs">
                <SideBook title="UP" book=up />
                <SideBook title="DOWN" book=down />
            </div>
        </div>
    }
}

#[component]
fn SideBook(title: &'static str, book: Signal<PolyBook>) -> impl IntoView {
    let asks = Signal::derive(move || {
        let mut a = book.with(|b| b.asks.clone());
        a.reverse();
        a
    });
    let bids = Signal::derive(move || book.with(|b| b.bids.clone()));
    let best_bid = Signal::derive(move || fmt_price(book.with(|b| b.best_bid)));
    let best_ask = Signal::derive(move || fmt_price(book.with(|b| b.best_ask)));
    let last_price = Signal::derive(move || fmt_price(book.with(|b| b.last_trade_price)));
    let last_side = Signal::derive(move || book.with(|b| b.last_trade_side.clone()));

    view! {
        <div>
            <div class="flex justify-between text-[10px] text-zinc-500 uppercase tracking-wider mb-1">
                <span>{title}</span>
                <span>"qty"</span>
            </div>
            <div class="space-y-0.5 numeric">
                <Levels levels=asks buy=false />
                <div class="flex justify-between border-y border-border py-1 text-[11px] text-zinc-300">
                    <span class="cell-bid">{move || best_bid.get()}</span>
                    <span class="text-zinc-500">"⇄"</span>
                    <span class="cell-ask">{move || best_ask.get()}</span>
                </div>
                <Levels levels=bids buy=true />
            </div>
            <div class="mt-2 text-[10px] text-zinc-500">
                "last "
                <span class="text-zinc-300">{move || last_price.get()}</span>
                " · "
                <span>{move || last_side.get()}</span>
            </div>
        </div>
    }
}

#[component]
fn Levels(levels: Signal<Vec<Level>>, buy: bool) -> impl IntoView {
    let cls = if buy {
        "flex justify-between cell-bid"
    } else {
        "flex justify-between cell-ask"
    };
    view! {
        <div>
            <For
                each={move || levels.get().into_iter().enumerate().collect::<Vec<(usize, Level)>>()}
                key={|(i, l): &(usize, Level)| (*i, (l.price * 10000.0) as i64)}
                children={move |(_, l): (usize, Level)| view! {
                    <div class={cls}>
                        <span>{fmt_price(l.price)}</span>
                        <span class="text-zinc-400">{fmt_qty(l.qty)}</span>
                    </div>
                }}
            />
        </div>
    }
}
