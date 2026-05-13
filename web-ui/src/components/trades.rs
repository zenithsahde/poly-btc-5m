use leptos::prelude::*;
use shared_types::TradeRow;

use crate::app::SnapshotSignal;
use crate::format::{fmt_price, fmt_qty};

#[component]
pub fn TradesPanel() -> impl IntoView {
    let SnapshotSignal(snapshot) = use_context().expect("SnapshotSignal");
    let trades =
        move || snapshot.with(|s| s.as_ref().map(|s| s.trades.clone()).unwrap_or_default());

    view! {
        <div class="card">
            <div class="card-title">"Recent Trades"</div>
            <div class="space-y-0.5 numeric text-xs max-h-72 overflow-y-auto pr-1">
                <For
                    each={move || trades().into_iter().enumerate().collect::<Vec<(usize, TradeRow)>>()}
                    key={|(i, t): &(usize, TradeRow)| (*i, t.dir_time.clone(), (t.price * 100.0) as i64)}
                    children={move |(_, t): (usize, TradeRow)| {
                        let cls = if t.is_buy { "flex justify-between text-zinc-300 cell-bid" } else { "flex justify-between text-zinc-300 cell-ask" };
                        view! {
                            <div class={cls}>
                                <span>{t.dir_time.clone()}</span>
                                <span>{fmt_price(t.price)}</span>
                                <span class="text-zinc-500">{fmt_qty(t.qty)}</span>
                                <span class="text-zinc-400">{format!("{:.0}", t.notional)}</span>
                            </div>
                        }
                    }}
                />
            </div>
        </div>
    }
}
