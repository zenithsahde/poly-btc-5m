use leptos::prelude::*;
use shared_types::Level;

use crate::app::SnapshotSignal;
use crate::format::{fmt_price, fmt_qty};

#[component]
pub fn OrderbookPanel() -> impl IntoView {
    let SnapshotSignal(snapshot) = use_context().expect("SnapshotSignal");

    let mid = move || snapshot.with(|s| s.as_ref().map(|s| s.orderbook.mid_price).unwrap_or(0.0));
    let spread = move || snapshot.with(|s| s.as_ref().map(|s| s.orderbook.spread_bps).unwrap_or(0.0));
    let bids = move || snapshot.with(|s| s.as_ref().map(|s| s.orderbook.bids.clone()).unwrap_or_default());
    let asks = move || snapshot.with(|s| s.as_ref().map(|s| s.orderbook.asks.clone()).unwrap_or_default());

    view! {
        <div class="card">
            <div class="flex items-center justify-between mb-3">
                <div class="card-title mb-0">"Binance · BTC L2"</div>
                <div class="text-xs text-zinc-500 numeric">
                    "spread "
                    <span class="text-zinc-300">{move || format!("{:.2}", spread())}</span>
                    " bps"
                </div>
            </div>
            <div class="grid grid-cols-2 gap-3 numeric text-xs">
                <div>
                    <div class="text-[10px] text-zinc-500 uppercase tracking-wider mb-1 flex justify-between">
                        <span>"BIDS"</span><span>"qty"</span>
                    </div>
                    <div class="space-y-0.5">
                        <For
                            each={move || bids().into_iter().enumerate().collect::<Vec<(usize, Level)>>()}
                            key={|(i, l): &(usize, Level)| (*i, (l.price * 100.0) as i64)}
                            children={move |(_, l): (usize, Level)| view! {
                                <div class="flex justify-between cell-bid">
                                    <span>{fmt_price(l.price)}</span>
                                    <span class="text-zinc-400">{fmt_qty(l.qty)}</span>
                                </div>
                            }}
                        />
                    </div>
                </div>
                <div>
                    <div class="text-[10px] text-zinc-500 uppercase tracking-wider mb-1 flex justify-between">
                        <span>"ASKS"</span><span>"qty"</span>
                    </div>
                    <div class="space-y-0.5">
                        <For
                            each={move || asks().into_iter().enumerate().collect::<Vec<(usize, Level)>>()}
                            key={|(i, l): &(usize, Level)| (*i, (l.price * 100.0) as i64)}
                            children={move |(_, l): (usize, Level)| view! {
                                <div class="flex justify-between cell-ask">
                                    <span>{fmt_price(l.price)}</span>
                                    <span class="text-zinc-400">{fmt_qty(l.qty)}</span>
                                </div>
                            }}
                        />
                    </div>
                </div>
            </div>
            <div class="mt-3 text-center text-sm border-t border-border pt-2 numeric">
                "mid "
                <span class="text-zinc-100 font-semibold">{move || fmt_price(mid())}</span>
            </div>
        </div>
    }
}
