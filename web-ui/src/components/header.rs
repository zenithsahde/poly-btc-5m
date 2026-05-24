use leptos::prelude::*;
use shared_types::OrderEventView;

use crate::app::SnapshotSignal;
use crate::format::{fmt_money, fmt_ms, fmt_qty, fmt_ts_ms, fmt_us};

const PNL_W: f64 = 150.0;
const PNL_H: f64 = 34.0;
const PNL_PAD: f64 = 3.0;

#[component]
pub fn HeaderBar() -> impl IntoView {
    let SnapshotSignal(snapshot) = use_context().expect("SnapshotSignal");
    let net = Signal::derive(move || {
        snapshot.with(|s| s.as_ref().map(|s| s.position.net_pnl).unwrap_or(0.0))
    });
    let net_text = Signal::derive(move || fmt_money(net.get()));
    let net_class = Signal::derive(move || {
        let v = net.get();
        if v > 0.0 {
            "top-pnl-value cell-bid"
        } else if v < 0.0 {
            "top-pnl-value cell-ask"
        } else {
            "top-pnl-value text-zinc-200"
        }
    });
    let avg_sum = Signal::derive(move || {
        snapshot.with(|s| {
            s.as_ref()
                .map(|s| format!("{:.3}", s.position.avg_sum))
                .unwrap_or_else(|| "-".to_string())
        })
    });
    let position_qty = Signal::derive(move || {
        snapshot.with(|s| {
            s.as_ref()
                .map(|s| {
                    format!(
                        "U {} / D {}",
                        fmt_qty(s.position.up.qty),
                        fmt_qty(s.position.down.qty)
                    )
                })
                .unwrap_or_else(|| "U - / D -".to_string())
        })
    });
    let cash_text = Signal::derive(move || {
        snapshot.with(|s| {
            s.as_ref()
                .map(|s| fmt_money(s.position.cash_pnl))
                .unwrap_or_else(|| "-".to_string())
        })
    });
    let inv_text = Signal::derive(move || {
        snapshot.with(|s| {
            s.as_ref()
                .map(|s| format!("{:.3}", s.position.inventory_value))
                .unwrap_or_else(|| "-".to_string())
        })
    });
    let action = Signal::derive(move || {
        snapshot.with(|s| {
            let Some(s) = s.as_ref() else {
                return "waiting for stream".to_string();
            };
            if let Some(action) = s.position.last_action.as_ref() {
                return action.clone();
            }
            if let Some(order) = s.position.ioc_last_order.as_ref() {
                return order.clone();
            }
            let up = s
                .position
                .maker_buy_intent_up
                .map(|(p, q, _)| format!("UP maker pending {:.0}@{:.2}", q, p));
            let down = s
                .position
                .maker_buy_intent_down
                .map(|(p, q, _)| format!("DOWN maker pending {:.0}@{:.2}", q, p));
            match (up, down) {
                (Some(u), Some(d)) => format!("{} | {}", u, d),
                (Some(u), None) => u,
                (None, Some(d)) => d,
                (None, None) => "no recent order action".to_string(),
            }
        })
    });
    let pnl_path = Signal::derive(move || {
        snapshot.with(|s| {
            let Some(s) = s.as_ref() else {
                return String::new();
            };
            pnl_sparkline_path(&s.position.pnl_history)
        })
    });
    let pnl_range = Signal::derive(move || {
        snapshot.with(|s| {
            let Some(s) = s.as_ref() else {
                return String::new();
            };
            let pts = &s.position.pnl_history;
            if pts.len() < 2 {
                return "pnl curve warming".to_string();
            }
            let lo = pts.iter().map(|p| p.net_pnl).fold(f64::INFINITY, f64::min);
            let hi = pts
                .iter()
                .map(|p| p.net_pnl)
                .fold(f64::NEG_INFINITY, f64::max);
            format!("{}..{}", fmt_money(lo), fmt_money(hi))
        })
    });
    let action_events = Signal::derive(move || {
        snapshot.with(|s| {
            s.as_ref()
                .map(|s| {
                    s.position
                        .order_events
                        .iter()
                        .take(5)
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
    });

    let mode_label = Signal::derive(move || {
        let live = snapshot.with(|s| s.as_ref().map(|s| s.header.is_live_mode).unwrap_or(false));
        if live {
            "Polymarket BTC 5m · 🔴 LIVE"
        } else {
            "Polymarket BTC 5m · ⚫ DRY-RUN"
        }
    });
    let mode_class = Signal::derive(move || {
        let live = snapshot.with(|s| s.as_ref().map(|s| s.header.is_live_mode).unwrap_or(false));
        if live {
            "text-xs text-red-400 uppercase tracking-widest font-semibold"
        } else {
            "text-xs text-zinc-500 uppercase tracking-widest"
        }
    });

    view! {
        <header class="topbar">
            <div class="topbar-main">
                <div class="brand-block">
                    <div class="text-xl font-bold tracking-tight">"rust_engine"</div>
                    <div class=move || mode_class.get()>
                        {move || mode_label.get()}
                    </div>
                </div>
                <div class="top-stats">
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
                    <Stat label="P99" value={Signal::derive(move || {
                        snapshot.with(|s| s.as_ref().map(|s| fmt_ms(s.latency.p99_ms)).unwrap_or_else(|| "—".into()))
                    })}/>
                    <Stat label="Parse" value={Signal::derive(move || {
                        snapshot.with(|s| s.as_ref().map(|s| fmt_us(s.latency.parse_p99_us)).unwrap_or_else(|| "—".into()))
                    })}/>
                    <Stat label="Total" value={Signal::derive(move || {
                        snapshot.with(|s| s.as_ref().map(|s| format!("{}", s.header.total_msgs)).unwrap_or_else(|| "—".into()))
                    })}/>
                    <Stat label="Uptime" value={Signal::derive(move || {
                        snapshot.with(|s| s.as_ref().map(|s| s.header.uptime.clone()).unwrap_or_else(|| "—".into()))
                    })}/>
                </div>
            </div>

            <div class="topbar-detail">
                <div class="top-pnl-strip">
                    <div class="top-pnl-main">
                        <span class="top-label">"net pnl"</span>
                        <span class={move || net_class.get()}>{move || net_text.get()}</span>
                    </div>
                    <div class="top-pnl-chart">
                        <svg viewBox={format!("0 0 {} {}", PNL_W, PNL_H)} class="top-pnl-svg">
                            <path d={move || pnl_path.get()} fill="none" stroke="currentColor" stroke-width="1.7"/>
                        </svg>
                        <span>{move || pnl_range.get()}</span>
                    </div>
                    <div class="top-pnl-meta">
                        <div><span>"range"</span><b>{move || pnl_range.get()}</b></div>
                        <div><span>"avg sum"</span><b>{move || avg_sum.get()}</b></div>
                        <div><span>"pos"</span><b>{move || position_qty.get()}</b></div>
                        <div><span>"cash"</span><b>{move || cash_text.get()}</b></div>
                        <div><span>"inv"</span><b>{move || inv_text.get()}</b></div>
                    </div>
                </div>
                <div class="top-action">
                    <div class="top-action-head">
                        <span class="top-label">"last"</span>
                        <span class="top-action-text">{move || action.get()}</span>
                    </div>
                    <div class="top-action-feed">
                        {move || {
                            let rows = action_events.get();
                            if rows.is_empty() {
                                view! { <div class="action-empty">"waiting for strategy actions"</div> }.into_any()
                            } else {
                                rows.into_iter()
                                    .map(|row| view! { <ActionRow event=row /> })
                                    .collect_view()
                                    .into_any()
                            }
                        }}
                    </div>
                </div>
            </div>
        </header>
    }
}

#[component]
fn ActionRow(event: OrderEventView) -> impl IntoView {
    let kind_class = match event.kind.as_str() {
        "FILL" | "FILL_TAKER" | "FILL_MAKER" | "MATCH" => "action-kind action-fill",
        "POST" | "ORDER" => "action-kind action-post",
        "CANCEL" => "action-kind action-cancel",
        "REJECT" => "action-kind action-reject",
        _ => "action-kind",
    };
    let pnl_class = if event.pnl > 0.0 {
        "action-pnl cell-bid"
    } else if event.pnl < 0.0 {
        "action-pnl cell-ask"
    } else {
        "action-pnl"
    };
    view! {
        <div class="action-row">
            <span class="action-time">{fmt_ts_ms(event.ts_ms)}</span>
            <span class={kind_class}>{event.kind}</span>
            <span class="action-liquidity">{event.liquidity}</span>
            <span class="action-summary">{event.summary}</span>
            <span class={pnl_class}>{fmt_money(event.pnl)}</span>
        </div>
    }
}

fn pnl_sparkline_path(points: &[shared_types::PnlPointView]) -> String {
    if points.len() < 2 {
        return String::new();
    }
    let xmin = points.first().map(|p| p.uptime_secs as f64).unwrap_or(0.0);
    let xmax = points.last().map(|p| p.uptime_secs as f64).unwrap_or(xmin);
    let xspan = (xmax - xmin).max(1e-6);
    let ymin = points
        .iter()
        .map(|p| p.net_pnl)
        .fold(f64::INFINITY, f64::min);
    let ymax = points
        .iter()
        .map(|p| p.net_pnl)
        .fold(f64::NEG_INFINITY, f64::max);
    let yspan = (ymax - ymin).max(1e-6);

    let mut d = String::new();
    for (idx, point) in points.iter().enumerate() {
        let x = PNL_PAD + ((point.uptime_secs as f64 - xmin) / xspan) * (PNL_W - 2.0 * PNL_PAD);
        let y = PNL_PAD + (1.0 - (point.net_pnl - ymin) / yspan) * (PNL_H - 2.0 * PNL_PAD);
        if idx == 0 {
            d.push_str(&format!("M{:.1},{:.1}", x, y));
        } else {
            d.push_str(&format!(" L{:.1},{:.1}", x, y));
        }
    }
    d
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
