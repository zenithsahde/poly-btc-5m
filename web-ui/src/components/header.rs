use leptos::prelude::*;

use crate::app::SnapshotSignal;

#[component]
pub fn HeaderBar() -> impl IntoView {
    let SnapshotSignal(snapshot) = use_context().expect("SnapshotSignal");

    view! {
        <header class="flex items-center justify-between px-6 py-3 border-b border-border bg-card/50 backdrop-blur">
            <div class="flex items-center gap-4">
                <div class="text-xl font-bold tracking-tight">"rust_engine"</div>
                <div class="text-xs text-zinc-500 uppercase tracking-widest">
                    "Polymarket BTC 5m · Live"
                </div>
            </div>
            <div class="flex items-center gap-6 text-sm numeric">
                <div class="flex items-center gap-1.5">
                    <span class={move || {
                        let on = snapshot.with(|s| s.as_ref().map(|s| s.header.ws_connected).unwrap_or(false));
                        if on { "dot dot-on" } else { "dot dot-off" }
                    }}></span>
                    <span class="text-xs text-zinc-400 uppercase tracking-wider">"Binance"</span>
                </div>
                <div class="flex items-center gap-1.5">
                    <span class={move || {
                        let on = snapshot.with(|s| s.as_ref().map(|s| s.header.poly_ws_connected).unwrap_or(false));
                        if on { "dot dot-on" } else { "dot dot-off" }
                    }}></span>
                    <span class="text-xs text-zinc-400 uppercase tracking-wider">"Polymarket"</span>
                </div>
                <Stat label="Symbol" value={Signal::derive(move || {
                    snapshot.with(|s| s.as_ref().map(|s| s.header.symbol.clone()).unwrap_or_default())
                })}/>
                <Stat label="Msg/s" value={Signal::derive(move || {
                    snapshot.with(|s| s.as_ref().map(|s| format!("{:.1}", s.header.msg_rate)).unwrap_or_else(|| "—".into()))
                })}/>
                <Stat label="Total" value={Signal::derive(move || {
                    snapshot.with(|s| s.as_ref().map(|s| format!("{}", s.header.total_msgs)).unwrap_or_else(|| "—".into()))
                })}/>
                <Stat label="Uptime" value={Signal::derive(move || {
                    snapshot.with(|s| s.as_ref().map(|s| s.header.uptime.clone()).unwrap_or_else(|| "—".into()))
                })}/>
            </div>
        </header>
    }
}

#[component]
fn Stat(label: &'static str, value: Signal<String>) -> impl IntoView {
    view! {
        <div class="flex flex-col items-end leading-tight">
            <span class="text-[10px] text-zinc-500 uppercase tracking-wider">{label}</span>
            <span class="text-sm text-zinc-100 numeric">{move || value.get()}</span>
        </div>
    }
}
