use leptos::prelude::*;

use crate::app::SnapshotSignal;
use crate::format::{fmt_pct_bps, fmt_price};

#[component]
pub fn FairValuePanel() -> impl IntoView {
    let SnapshotSignal(snapshot) = use_context().expect("SnapshotSignal");

    let fair = Signal::derive(move || {
        format!(
            "{:.4}",
            snapshot.with(|s| s.as_ref().map(|s| s.fair_value.fair_price).unwrap_or(0.0))
        )
    });
    let fair_down = Signal::derive(move || {
        format!(
            "{:.4}",
            snapshot.with(|s| s
                .as_ref()
                .map(|s| s.fair_value.fair_price_down)
                .unwrap_or(0.0))
        )
    });
    let vol = Signal::derive(move || {
        format!(
            "{:.3}",
            snapshot.with(|s| s
                .as_ref()
                .map(|s| s.fair_value.volatility_annual)
                .unwrap_or(0.0))
        )
    });
    let sigma_src = Signal::derive(move || {
        snapshot.with(|s| {
            s.as_ref()
                .map(|s| s.fair_value.sigma_source.clone())
                .unwrap_or_default()
        })
    });
    let state = Signal::derive(move || {
        snapshot.with(|s| {
            s.as_ref()
                .map(|s| s.fair_value.market_state.clone())
                .unwrap_or_else(|| "—".into())
        })
    });
    let gap = Signal::derive(move || {
        fmt_pct_bps(snapshot.with(|s| {
            s.as_ref()
                .map(|s| s.fair_value.signal_gap_bps)
                .unwrap_or(0.0)
        }))
    });
    let snipe = Signal::derive(move || {
        snapshot.with(|s| {
            s.as_ref()
                .map(|s| s.fair_value.last_snipe_info.clone())
                .unwrap_or_default()
        })
    });
    let snipes = Signal::derive(move || {
        format!(
            "{}",
            snapshot.with(|s| s.as_ref().map(|s| s.fair_value.snipe_count).unwrap_or(0))
        )
    });
    let strike = Signal::derive(move || {
        fmt_price(snapshot.with(|s| s.as_ref().map(|s| s.fair_value.strike_price).unwrap_or(0.0)))
    });
    let expiry = Signal::derive(move || {
        format!(
            "{:.2} min",
            snapshot.with(|s| s
                .as_ref()
                .map(|s| s.fair_value.expiry_minutes)
                .unwrap_or(0.0))
        )
    });
    let iv_poly = Signal::derive(move || {
        format!(
            "{:.3}",
            snapshot.with(|s| s.as_ref().map(|s| s.fair_value.iv_poly).unwrap_or(0.0))
        )
    });
    let chainlink = Signal::derive(move || {
        snapshot.with(|s| {
            s.as_ref()
                .and_then(|s| s.fair_value.chainlink_price)
                .map(|p| fmt_price(p))
                .unwrap_or_else(|| "—".into())
        })
    });
    let basis = Signal::derive(move || {
        snapshot.with(|s| {
            s.as_ref()
                .and_then(|s| s.fair_value.basis_vs_chainlink)
                .map(|b| format!("{:+.2}", b))
                .unwrap_or_else(|| "—".into())
        })
    });
    let jump_1s = Signal::derive(move || {
        snapshot.with(|s| {
            s.as_ref()
                .map(|s| format!("{:+.2}", s.fair_value.binance_jump_1s))
                .unwrap_or_else(|| "0.00".into())
        })
    });
    let jump_age = Signal::derive(move || {
        snapshot.with(|s| {
            s.as_ref()
                .and_then(|s| {
                    s.fair_value.last_jump_age_ms.map(|ms| {
                        let dir_label = match s.fair_value.last_jump_dir {
                            Some(1) => "↑",
                            Some(-1) => "↓",
                            _ => "·",
                        };
                        format!("{} {}ms ago", dir_label, ms)
                    })
                })
                .unwrap_or_else(|| "—".into())
        })
    });

    view! {
        <div class="card">
            <div class="flex items-center justify-between mb-3">
                <div class="card-title mb-0">"Fair Value · σ"</div>
                <span class="text-[10px] uppercase tracking-wider px-2 py-0.5 rounded bg-zinc-800/60">
                    {move || state.get()}
                </span>
            </div>

            <div class="grid grid-cols-2 gap-3 mb-4">
                <div>
                    <div class="text-[10px] text-zinc-500 uppercase tracking-wider">"FV UP"</div>
                    <div class="text-2xl font-semibold numeric cell-bid">{move || fair.get()}</div>
                </div>
                <div>
                    <div class="text-[10px] text-zinc-500 uppercase tracking-wider">"FV DOWN"</div>
                    <div class="text-2xl font-semibold numeric cell-ask">{move || fair_down.get()}</div>
                </div>
            </div>

            <div class="space-y-1">
                <Kv k="strike" v=strike />
                <Kv k="expiry" v=expiry />
                <Kv k="σ annual" v=vol />
                <Kv k="σ source" v=sigma_src />
                <Kv k="iv poly" v=iv_poly />
                <Kv k="gap" v=gap />
                <Kv k="snipes" v=snipes />
            </div>
            <div class="mt-3 pt-3 border-t border-border space-y-1">
                <div class="text-[10px] text-zinc-500 uppercase tracking-wider mb-1">"Chainlink (settle source) · Jump alpha"</div>
                <Kv k="chainlink $" v=chainlink />
                <Kv k="basis (BI-CL)" v=basis />
                <Kv k="jump 1s $" v=jump_1s />
                <Kv k="last jump" v=jump_age />
            </div>
            <div class="mt-3 pt-3 border-t border-border text-[11px] text-zinc-400 numeric">
                {move || snipe.get()}
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
