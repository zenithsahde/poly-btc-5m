use leptos::prelude::*;

use crate::app::SnapshotSignal;
use crate::format::{fmt_ms, fmt_us};

#[component]
pub fn LatencyPanel() -> impl IntoView {
    let SnapshotSignal(snapshot) = use_context().expect("SnapshotSignal");
    let p50 = Signal::derive(move || fmt_ms(snapshot.with(|s| s.as_ref().map(|s| s.latency.p50_ms).unwrap_or(0.0))));
    let p99 = Signal::derive(move || fmt_ms(snapshot.with(|s| s.as_ref().map(|s| s.latency.p99_ms).unwrap_or(0.0))));
    let max = Signal::derive(move || fmt_ms(snapshot.with(|s| s.as_ref().map(|s| s.latency.max_ms).unwrap_or(0.0))));
    let parse = Signal::derive(move || fmt_us(snapshot.with(|s| s.as_ref().map(|s| s.latency.parse_p99_us).unwrap_or(0.0))));
    let count = Signal::derive(move || format!("{}", snapshot.with(|s| s.as_ref().map(|s| s.latency.msg_count).unwrap_or(0))));
    let warn_present = move || snapshot.with(|s| s.as_ref().and_then(|s| s.latency.last_warn_ms).is_some());
    let warn_text = Signal::derive(move || {
        snapshot.with(|s| s.as_ref().and_then(|s| s.latency.last_warn_ms).map(fmt_ms).unwrap_or_default())
    });

    view! {
        <div class="card">
            <div class="card-title">"Latency"</div>
            <div class="space-y-1">
                <Kv k="P50" v=p50 />
                <Kv k="P99" v=p99 />
                <Kv k="Max" v=max />
                <Kv k="Parse P99" v=parse />
                <Kv k="Msgs" v=count />
                <Show when={move || warn_present()}>
                    <div class="kv text-danger">
                        <span class="kv-key">"Last warn"</span>
                        <span class="kv-val">{move || warn_text.get()}</span>
                    </div>
                </Show>
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
