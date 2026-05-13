//! 根组件：建立 SSE 连接 + 提供 snapshot signal context + 布局所有面板。

use leptos::prelude::*;
use shared_types::DashboardSnapshot;

use crate::components::{
    fair_value::FairValuePanel, header::HeaderBar, order_log::OrderLogPanel,
    orderbook::OrderbookPanel, poly_books::PolymarketPanel, position::PositionPanel,
    price_chart::PriceChart, trades::TradesPanel,
};
use crate::sse;

/// snapshot 信号：所有面板订阅它。
#[derive(Clone, Copy)]
pub struct SnapshotSignal(pub RwSignal<Option<DashboardSnapshot>>);

#[component]
pub fn App() -> impl IntoView {
    let snapshot = RwSignal::new(None::<DashboardSnapshot>);
    provide_context(SnapshotSignal(snapshot));

    // 价格历史（前端维持的滑动窗口，给图表用）
    let history = RwSignal::new(Vec::<(f64, f64)>::new()); // (uptime_secs, mid_price)
    provide_context(PriceHistory(history));
    Effect::new(move |_| {
        if let Some(s) = snapshot.get() {
            let mid = s.orderbook.mid_price;
            if mid > 0.0 {
                history.update(|h| {
                    h.push((s.header.uptime_secs as f64, mid));
                    // 保留最近 600 个采样点 (~60s @100ms)
                    let len = h.len();
                    if len > 600 {
                        h.drain(0..(len - 600));
                    }
                });
            }
        }
    });

    // SSE 连接：句柄需活到页面关闭。wasm_bindgen Closure 不是 Send+Sync，无法塞进 StoredValue；
    // 这里直接 leak — CSR 单页应用中 EventSource 与 tab 同寿命，无需早 drop。
    std::mem::forget(sse::connect(snapshot));

    view! {
        <div class="min-h-screen flex flex-col">
            <HeaderBar />
            <main class="dashboard-main">
                <div class="dashboard-grid">
                    <section class="dash-orderbook"><OrderbookPanel /></section>
                    <section class="dash-chart"><PriceChart /></section>
                    <section class="dash-poly"><PolymarketPanel /></section>
                    <section class="dash-trades"><TradesPanel /></section>
                    <section class="dash-fair"><FairValuePanel /></section>
                    <section class="dash-position"><PositionPanel /></section>
                    <section class="dash-orders"><OrderLogPanel /></section>
                </div>
            </main>
            <footer class="px-6 py-3 text-xs text-zinc-500 border-t border-border">
                "rust_engine · live via SSE @ /api/stream"
            </footer>
        </div>
    }
}

#[derive(Clone, Copy)]
pub struct PriceHistory(pub RwSignal<Vec<(f64, f64)>>);
