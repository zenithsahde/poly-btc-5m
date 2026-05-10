//! 简易 SVG 折线图：mid price 时间序列。后续可用 rust-ui.com 图表组件替换。

use leptos::prelude::*;

use crate::app::{PriceHistory, SnapshotSignal};
use crate::format::fmt_price;

const W: f64 = 480.0;
const H: f64 = 160.0;
const PAD: f64 = 8.0;

#[component]
pub fn PriceChart() -> impl IntoView {
    let SnapshotSignal(snapshot) = use_context().expect("SnapshotSignal");
    let PriceHistory(history) = use_context().expect("PriceHistory");

    let mid_now = Signal::derive(move || fmt_price(snapshot.with(|s| s.as_ref().map(|s| s.orderbook.mid_price).unwrap_or(0.0))));

    let path_d = Signal::derive(move || {
        let pts = history.get();
        if pts.len() < 2 {
            return String::new();
        }
        let xs: Vec<f64> = pts.iter().map(|(t, _)| *t).collect();
        let ys: Vec<f64> = pts.iter().map(|(_, p)| *p).collect();
        let xmin = *xs.first().unwrap();
        let xmax = *xs.last().unwrap();
        let xspan = (xmax - xmin).max(1e-6);
        let ymin = ys.iter().cloned().fold(f64::INFINITY, f64::min);
        let ymax = ys.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let yspan = (ymax - ymin).max(1e-6);
        let mut d = String::new();
        for (i, (t, p)) in pts.iter().enumerate() {
            let x = PAD + (t - xmin) / xspan * (W - 2.0 * PAD);
            let y = PAD + (1.0 - (p - ymin) / yspan) * (H - 2.0 * PAD);
            if i == 0 {
                d.push_str(&format!("M{:.1},{:.1}", x, y));
            } else {
                d.push_str(&format!(" L{:.1},{:.1}", x, y));
            }
        }
        d
    });

    let range_text = Signal::derive(move || {
        let pts = history.get();
        if pts.is_empty() { return String::new(); }
        let ys: Vec<f64> = pts.iter().map(|(_, p)| *p).collect();
        let lo = ys.iter().cloned().fold(f64::INFINITY, f64::min);
        let hi = ys.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        if lo > 0.0 { format!("range {:.0} – {:.0}", lo, hi) } else { String::new() }
    });

    view! {
        <div class="card">
            <div class="flex items-center justify-between mb-3">
                <div class="card-title mb-0">
                    "BTC Mid · "
                    <span class="text-zinc-300 numeric">{move || mid_now.get()}</span>
                </div>
                <div class="text-[11px] text-zinc-500 numeric">{move || range_text.get()}</div>
            </div>
            <svg viewBox={format!("0 0 {} {}", W, H)} class="w-full h-40">
                <rect x="0" y="0" width={format!("{W}")} height={format!("{H}")} fill="transparent"/>
                <path d={move || path_d.get()} fill="none" stroke="#3b82f6" stroke-width="1.5"/>
            </svg>
        </div>
    }
}
