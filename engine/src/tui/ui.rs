/// tui/ui.rs - TUI 界面渲染
///
/// 布局（全终端宽度）：
/// ┌─────────────────────────────────────────────────────────────────┐
/// │  SJ Engine  BTCUSDT  ●连接  msg=1247/s  up=00:02:13            │  ← 标题栏
/// ├────────────────────┬──────────────────────┬─────────────────────┤
/// │  订单簿            │  实时成交流          │  延迟统计           │
/// │  卖单 (asks)       │  时间  方向  价格    │  P50  P99  Max      │
/// │  ───────────────   │  ─────────────────── │  ─────────────────  │
/// │  67911.00   0.50   │  08:12  买▲ 67910.5  │  2.1ms 5.8ms 14ms  │
/// │  67910.80   1.20   │  08:12  卖▼ 67910.4  │                     │
/// │  67910.60   2.30   │  ...                 │  解析延迟:          │
/// │  ══ 67910.49/50 ══ │                      │  P99=380μs          │
/// │  67910.50   3.07   │                      │                     │
/// │  67910.30   0.80   │                      │  消息速率:          │
/// │  67910.00   1.50   │                      │  1247 msg/s         │
/// └────────────────────┴──────────────────────┴─────────────────────┘
/// │  状态栏：最新延迟警告                                           │
/// └─────────────────────────────────────────────────────────────────┘
use chrono::TimeZone;
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
    Frame,
};
use std::sync::RwLock;

use crate::position::{ManagedOrder, PendingOrderReason};
use crate::tui::app::{AppState, ChaseSide};

fn pending_reason_label(reason: PendingOrderReason) -> &'static str {
    match reason {
        PendingOrderReason::Chase => "chase",
        PendingOrderReason::Rebalance => "rebal",
    }
}

fn format_pending_buy(order: &ManagedOrder) -> String {
    format!(
        "买@{:.2}×{:.0}/{}",
        order.price,
        order.remaining_qty(),
        pending_reason_label(order.reason)
    )
}

/// 主渲染函数：标题 + 左侧 Binance（订单簿+成交+延迟）+ 右侧 Poly Up/Down 双订单簿 + 状态栏
/// v0.4.9: snapshot-clone 模式 — 持锁仅 ~微秒，立即释放后再绘制，消除与 signal.rs write 锁竞争
pub fn render(frame: &mut Frame, state: &RwLock<AppState>) {
    let s: AppState = {
        let guard = state.read().unwrap();
        guard.clone()
    };
    // 锁已释放，后续 render 操作完全不阻塞 signal.rs writer

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(22),
            Constraint::Length(3),
        ])
        .split(frame.area());

    render_title(frame, &s, outer[0]);

    let main_row = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(36), Constraint::Percentage(64)])
        .split(outer[1]);

    let left_col = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(6),
            Constraint::Min(4),
            Constraint::Length(8),
            Constraint::Min(14),
        ])
        .split(main_row[0]);
    render_orderbook(frame, &s, left_col[0]);
    render_trades(frame, &s, left_col[1]);
    render_latency(frame, &s, left_col[2]);
    render_fair_value_panel(frame, &s, left_col[3]);

    let right_col = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(18), Constraint::Length(14)])
        .split(main_row[1]);
    render_poly_panel(frame, &s, right_col[0]);
    render_position_panel(frame, &s, right_col[1]);
    render_statusbar(frame, &s, outer[2]);
}

/// 标题栏
fn render_title(frame: &mut Frame, s: &AppState, area: ratatui::layout::Rect) {
    let conn_indicator = if s.ws_connected {
        Span::styled("● 已连接", Style::default().fg(Color::Green))
    } else {
        Span::styled(
            "○ 断线中",
            Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::SLOW_BLINK),
        )
    };

    let title = Line::from(vec![
        Span::styled(
            "  🦀 SJ Engine  ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            &s.symbol,
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        conn_indicator,
        Span::styled(
            format!("  {:.0} msg/s", s.msg_rate),
            Style::default().fg(Color::White),
        ),
        Span::styled(
            format!("  up={}", s.uptime()),
            Style::default().fg(Color::DarkGray),
        ),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));

    let para = Paragraph::new(title)
        .block(block)
        .alignment(Alignment::Left);

    frame.render_widget(para, area);
}

/// 订单簿面板：卖在上、中间价一行、买在下，价格/数量右对齐
fn render_orderbook(frame: &mut Frame, s: &AppState, area: ratatui::layout::Rect) {
    let block = Block::default()
        .title(" 📒 订单簿 L2 ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::White));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let w_price = 10;
    let w_qty = 10;

    let header = Row::new(vec![
        Cell::from(format!("{:>w_price$}", "卖价")).style(
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from(format!("{:>w_qty$}", "数量")).style(
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    let ask_rows: Vec<Row> = s
        .asks
        .iter()
        .rev()
        .take(8)
        .map(|lv| {
            Row::new(vec![
                Cell::from(format!("{:>w_price$.2}", lv.price))
                    .style(Style::default().fg(Color::Red)),
                Cell::from(format!("{:>w_qty$.4}", lv.qty))
                    .style(Style::default().fg(Color::White)),
            ])
        })
        .collect();
    let mid_row = Row::new(vec![
        Cell::from(format!(
            "{:>w_price$.2} │ {:>w_price$.2}",
            s.best_bid, s.best_ask
        ))
        .style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("── mid ──").style(Style::default().fg(Color::DarkGray)),
    ]);
    let bid_header = Row::new(vec![
        Cell::from(format!("{:>w_price$}", "买价")).style(
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from(format!("{:>w_qty$}", "数量")).style(
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    let bid_rows: Vec<Row> = s
        .bids
        .iter()
        .take(8)
        .map(|lv| {
            Row::new(vec![
                Cell::from(format!("{:>w_price$.2}", lv.price))
                    .style(Style::default().fg(Color::Green)),
                Cell::from(format!("{:>w_qty$.4}", lv.qty))
                    .style(Style::default().fg(Color::White)),
            ])
        })
        .collect();

    let mut rows = vec![header];
    rows.extend(ask_rows);
    rows.push(mid_row);
    rows.push(bid_header);
    rows.extend(bid_rows);

    let table = Table::new(
        rows,
        [
            Constraint::Length((w_price + 2) as u16),
            Constraint::Length((w_qty + 2) as u16),
        ],
    )
    .column_spacing(1);
    frame.render_widget(table, inner);
}

/// 实时成交流面板
fn render_trades(frame: &mut Frame, s: &AppState, area: ratatui::layout::Rect) {
    let block = Block::default()
        .title(" ⚡ 实时成交 ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::White));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // 4列：方向+时间 / 价格 / 数量 / 金额
    let header = Row::new(vec![
        Cell::from("方向 时间 ").style(
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("价格").style(
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("数量").style(
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("金额").style(
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        ),
    ]);

    let rows: Vec<Row> = s
        .recent_trades
        .iter()
        .map(|t| {
            // 买=绿色文字，卖=红色文字
            let dir_color = if t.is_buy { Color::Green } else { Color::Red };

            // 大单高亮
            let notional_style = if t.notional > 100_000.0 {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };

            Row::new(vec![
                Cell::from(t.dir_time.as_str())
                    .style(Style::default().fg(dir_color).add_modifier(Modifier::BOLD)),
                Cell::from(format!("{:.2}", t.price)).style(Style::default().fg(Color::White)),
                Cell::from(format!("{:.4}", t.qty)).style(Style::default().fg(Color::White)),
                Cell::from(format!("{:.0}U", t.notional)).style(notional_style),
            ])
        })
        .collect();

    let mut all_rows = vec![header];
    all_rows.extend(rows);

    // 列宽：方向+时间(12) / 价格(Min10) / 数量(Min8) / 金额(Min6)
    let col_widths = [
        Constraint::Length(12),
        Constraint::Min(10),
        Constraint::Min(8),
        Constraint::Min(6),
    ];

    let table = Table::new(all_rows, col_widths).column_spacing(1);
    frame.render_widget(table, inner);
}

/// 延迟统计面板
fn render_latency(frame: &mut Frame, s: &AppState, area: ratatui::layout::Rect) {
    // 根据 P99 延迟变色
    let border_color = if s.latency.p99_ms > 20.0 {
        Color::Red
    } else if s.latency.p99_ms > 10.0 {
        Color::Yellow
    } else {
        Color::Green
    };

    let block = Block::default()
        .title(" 📊 延迟统计 ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let p50_color = if s.latency.p50_ms < 5.0 {
        Color::Green
    } else if s.latency.p50_ms < 10.0 {
        Color::Yellow
    } else {
        Color::Red
    };
    let p99_color = if s.latency.p99_ms < 10.0 {
        Color::Green
    } else if s.latency.p99_ms < 20.0 {
        Color::Yellow
    } else {
        Color::Red
    };

    let text = Text::from(vec![
        Line::from("─── 币安 网络延迟 ───"),
        Line::from(vec![
            Span::raw("  P50 : "),
            Span::styled(
                format!("{:.2} ms", s.latency.p50_ms),
                Style::default().fg(p50_color).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::raw("  P99 : "),
            Span::styled(
                format!("{:.2} ms", s.latency.p99_ms),
                Style::default().fg(p99_color).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::raw("  Max : "),
            Span::styled(
                format!("{:.2} ms", s.latency.max_ms),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(""),
        Line::from("─── 解析延迟 ───"),
        Line::from(vec![
            Span::raw("  P99 : "),
            Span::styled(
                format!("{:.2} ms", s.latency.parse_p99_us / 1000.0),
                Style::default().fg(Color::Cyan),
            ),
        ]),
        Line::from(""),
        Line::from("─── 吞吐 ───────"),
        Line::from(vec![
            Span::raw("  速率: "),
            Span::styled(
                format!("{:.0} msg/s", s.msg_rate),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::raw("  总计: "),
            Span::styled(
                format!("{}", s.total_msgs),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
    ]);

    let para = Paragraph::new(text);
    frame.render_widget(para, inner);
}

/// Poly 5m：左 Up | 中 Spread/Last | 右 Down；卖(Asks)在上、中间 Last+Spread、买(Bids)在下（与参考 Order Book 一致）
fn render_poly_panel(frame: &mut Frame, s: &AppState, area: ratatui::layout::Rect) {
    let conn = if s.poly_ws_connected {
        Span::styled("●", Style::default().fg(Color::Green))
    } else {
        Span::styled("○", Style::default().fg(Color::Red))
    };
    let secs =
        crate::ws::discovery::MarketDiscovery::seconds_until_next_window(s.poly_window_end_ts);
    let countdown = format!("{}m {}s", secs / 60, secs % 60);
    let delay_str = s
        .poly_delay_ms()
        .map(|ms| format!("{} ms", ms))
        .unwrap_or_else(|| "—".to_string());
    let delay_color = match s.poly_delay_ms() {
        Some(ms) if ms > 2000 => Color::Red,
        Some(ms) if ms > 500 => Color::Yellow,
        _ => Color::Green,
    };

    let block = Block::default()
        .title(Line::from(vec![
            Span::raw(" 📗 Poly 5m  "),
            conn,
            Span::raw(" "),
            Span::styled(
                s.poly_market_slug.as_str(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  |  切换: "),
            Span::styled(countdown.as_str(), Style::default().fg(Color::Yellow)),
            Span::raw("  |  Poly延迟: "),
            Span::styled(
                delay_str.as_str(),
                Style::default()
                    .fg(delay_color)
                    .add_modifier(Modifier::BOLD),
            ),
        ]))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    // 挂单信息：用于在订单簿档位打标及底部汇总
    let up_buy = s.ledger.maker_buy_intent_up.as_ref();
    let down_buy = s.ledger.maker_buy_intent_down.as_ref();
    // v0.4.0-5m：sell 全部废弃，挂单只剩 maker buy（卖侧用 merge 退出）
    let up_sell: Option<(f64, f64)> = None;
    let down_sell: Option<(f64, f64)> = None;
    fn near(a: f64, b: f64) -> bool {
        (a - b).abs() < 0.006
    }

    let inner_split = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(14), Constraint::Length(2)])
        .split(inner);

    // Spread = best_ask - best_bid；asks 存升序(best=first)，bids 存降序(best=first)
    let up_spr = s
        .poly_asks
        .first()
        .zip(s.poly_bids.first())
        .map(|(a, b)| (a.price - b.price).max(0.0))
        .unwrap_or_else(|| (s.poly_best_ask - s.poly_best_bid).max(0.0));
    let down_spr = s
        .poly_down_asks
        .first()
        .zip(s.poly_down_bids.first())
        .map(|(a, b)| (a.price - b.price).max(0.0))
        .unwrap_or_else(|| (s.poly_down_best_ask - s.poly_down_best_bid).max(0.0));

    let header = Row::new(vec![
        Cell::from("  Up 价   量  ").style(
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("    Last   Spread   ").style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Cell::from("  Down 价   量  ").style(
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    let mut rows = vec![header];

    let n = 6;
    // 卖盘(Asks)：我们的挂卖在该档则打标 ←卖×数量
    for i in 0..n {
        let idx = n - 1 - i;
        let (up_s, up_our) = s
            .poly_asks
            .get(idx)
            .map(|l| {
                let our = up_sell.map(|(p, q)| near(l.price, p)).unwrap_or(false);
                let s = if our {
                    format!("{:>5.2} {:>7.2} ←卖×{}", l.price, l.qty, up_sell.unwrap().1)
                } else {
                    format!("{:>5.2} {:>7.2}", l.price, l.qty)
                };
                (s, our)
            })
            .unwrap_or_else(|| ("   —     —   ".to_string(), false));
        let (down_s, down_our) = s
            .poly_down_asks
            .get(idx)
            .map(|l| {
                let our = down_sell.map(|(p, _)| near(l.price, p)).unwrap_or(false);
                let s = if our {
                    format!(
                        "{:>5.2} {:>7.2} ←卖×{}",
                        l.price,
                        l.qty,
                        down_sell.unwrap().1
                    )
                } else {
                    format!("{:>5.2} {:>7.2}", l.price, l.qty)
                };
                (s, our)
            })
            .unwrap_or_else(|| ("   —     —   ".to_string(), false));
        rows.push(Row::new(vec![
            Cell::from(up_s).style(
                Style::default()
                    .fg(if up_our { Color::Yellow } else { Color::Red })
                    .add_modifier(if up_our {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Cell::from("").style(Style::default().fg(Color::DarkGray)),
            Cell::from(down_s).style(
                Style::default()
                    .fg(if down_our { Color::Yellow } else { Color::Red })
                    .add_modifier(if down_our {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
        ]));
    }
    // 中间一行：Last / Spread 与数值同列对齐，左右分隔线
    let mid_line = format!(
        " Up  {:>5.2}  {:>5.2}   │   Dn  {:>5.2}  {:>5.2} ",
        s.poly_last_trade_price, up_spr, s.poly_down_last_trade_price, down_spr
    );
    rows.push(Row::new(vec![
        Cell::from("  ——— Asks / Bids ———  ").style(Style::default().fg(Color::DarkGray)),
        Cell::from(mid_line).style(Style::default().fg(Color::Cyan)),
        Cell::from("  ——— Asks / Bids ———  ").style(Style::default().fg(Color::DarkGray)),
    ]));
    // 买盘(Bids)：我们的挂买在该档则打标 ←买×1
    for i in 0..n {
        let (up_s, up_our) = s
            .poly_bids
            .get(i)
            .map(|l| {
                let our = up_buy.map(|o| near(l.price, o.price)).unwrap_or(false);
                let s = if our {
                    format!(
                        "{:>5.2} {:>7.2} ←买×{}",
                        l.price,
                        l.qty,
                        up_buy.map(|o| o.remaining_qty()).unwrap_or(1.0)
                    )
                } else {
                    format!("{:>5.2} {:>7.2}", l.price, l.qty)
                };
                (s, our)
            })
            .unwrap_or_else(|| ("   —     —   ".to_string(), false));
        let (down_s, down_our) = s
            .poly_down_bids
            .get(i)
            .map(|l| {
                let our = down_buy.map(|o| near(l.price, o.price)).unwrap_or(false);
                let s = if our {
                    format!(
                        "{:>5.2} {:>7.2} ←买×{}",
                        l.price,
                        l.qty,
                        down_buy.map(|o| o.remaining_qty()).unwrap_or(1.0)
                    )
                } else {
                    format!("{:>5.2} {:>7.2}", l.price, l.qty)
                };
                (s, our)
            })
            .unwrap_or_else(|| ("   —     —   ".to_string(), false));
        rows.push(Row::new(vec![
            Cell::from(up_s).style(
                Style::default()
                    .fg(if up_our { Color::Cyan } else { Color::Green })
                    .add_modifier(if up_our {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
            Cell::from("").style(Style::default().fg(Color::DarkGray)),
            Cell::from(down_s).style(
                Style::default()
                    .fg(if down_our { Color::Cyan } else { Color::Green })
                    .add_modifier(if down_our {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    }),
            ),
        ]));
    }

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(35),
            Constraint::Length(48),
            Constraint::Percentage(35),
        ],
    )
    .column_spacing(1);
    frame.render_widget(table, inner_split[0]);

    // 底部挂单汇总：方向、价格、数量
    let up_buy_str = up_buy
        .map(format_pending_buy)
        .unwrap_or_else(|| "买—".to_string());
    let up_sell_str = up_sell
        .map(|(p, q)| format!("卖@{:.2}×{:.2}", p, q))
        .unwrap_or_else(|| "卖—".to_string());
    let down_buy_str = down_buy
        .map(format_pending_buy)
        .unwrap_or_else(|| "买—".to_string());
    let down_sell_str = down_sell
        .map(|(p, q)| format!("卖@{:.2}×{:.2}", p, q))
        .unwrap_or_else(|| "卖—".to_string());
    let footer_text = Line::from(vec![
        Span::styled(" 挂单 ", Style::default().fg(Color::DarkGray)),
        Span::styled("Up ", Style::default().fg(Color::Green)),
        Span::raw(up_buy_str),
        Span::raw(" "),
        Span::raw(up_sell_str),
        Span::styled("  │  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Down ", Style::default().fg(Color::Blue)),
        Span::raw(down_buy_str),
        Span::raw(" "),
        Span::raw(down_sell_str),
    ]);
    let footer = Paragraph::new(footer_text);
    frame.render_widget(footer, inner_split[1]);
}

/// 5 分钟公允价面板：当前窗口/倒计时、S/K/T/σ、FV_up/FV_down、偏差、合理性
fn render_fair_value_panel(frame: &mut Frame, s: &AppState, area: ratatui::layout::Rect) {
    let block = Block::default()
        .title(" 📐 5m 公允价 ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let secs =
        crate::ws::discovery::MarketDiscovery::seconds_until_next_window(s.poly_window_end_ts);
    let countdown = format!("{}m {}s", secs / 60, secs % 60);
    let window_end_str: String = if s.poly_window_end_ts > 0 {
        chrono::Utc
            .timestamp_opt(s.poly_window_end_ts, 0)
            .single()
            .map(|dt: chrono::DateTime<chrono::Utc>| dt.format("%H:%M UTC").to_string())
            .unwrap_or_else(|| "—".to_string())
    } else {
        "—".to_string()
    };

    let t_status = if s.poly_window_end_ts > 0 {
        format!("T 动态 {:.1} min", s.expiry_minutes)
    } else {
        "T 固定(不合理)".to_string()
    };
    let sigma_status = format!("σ {}", s.sigma_source);

    let text = Text::from(vec![
        Line::from(vec![
            Span::raw("窗口结束: "),
            Span::styled(window_end_str, Style::default().fg(Color::Cyan)),
            Span::raw("  切换: "),
            Span::styled(
                countdown.as_str(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw("  S(mid)="),
            Span::styled(
                format!("{:.2}", s.mid_price),
                Style::default().fg(Color::White),
            ),
            Span::raw("  K="),
            Span::styled(
                format!("{:.0}", s.strike_price),
                Style::default().fg(Color::White),
            ),
            Span::raw("  T="),
            Span::styled(
                format!("{:.1}min", s.expiry_minutes),
                Style::default().fg(Color::White),
            ),
            Span::raw("  σ="),
            Span::styled(
                format!("{:.1}%", s.volatility_annual * 100.0),
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::raw("  FV_up="),
            Span::styled(
                format!("{:.2}", s.fair_price),
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  FV_down="),
            Span::styled(
                format!("{:.2}", s.fair_price_down),
                Style::default()
                    .fg(Color::Blue)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::raw("  偏差="),
            Span::styled(
                format!("{:.0} bps", s.signal_gap_bps),
                Style::default().fg(Color::Yellow),
            ),
            Span::raw("  Poly IV="),
            Span::styled(
                if s.iv_poly >= 2.0 {
                    "≥200%".to_string()
                } else if s.iv_poly < 0.01 {
                    "—".to_string()
                } else {
                    format!("{:.0}%", s.iv_poly * 100.0)
                },
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(t_status, Style::default().fg(Color::Cyan)),
            Span::raw("  状态: "),
            Span::styled(
                s.market_state.as_str(),
                Style::default().fg(if s.market_state == "稳态" {
                    Color::Green
                } else {
                    Color::Yellow
                }),
            ),
            Span::raw("  "),
            Span::styled(&sigma_status, Style::default().fg(Color::Cyan)),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::raw("  正延迟(FV先): "),
            Span::styled(
                format!("n={} ", s.delay_stats.count_pos),
                Style::default().fg(Color::Cyan),
            ),
            Span::raw("min="),
            Span::styled(
                if s.delay_stats.count_pos > 0 {
                    format!("{:.0}ms", s.delay_stats.min_pos_ms)
                } else {
                    "—".to_string()
                },
                Style::default().fg(Color::White),
            ),
            Span::raw(" max="),
            Span::styled(
                if s.delay_stats.count_pos > 0 {
                    format!("{:.0}ms", s.delay_stats.max_pos_ms)
                } else {
                    "—".to_string()
                },
                Style::default().fg(Color::White),
            ),
            Span::raw(" avg="),
            Span::styled(
                if s.delay_stats.count_pos > 0 {
                    format!(
                        "{:.0}ms",
                        s.delay_stats.sum_pos_ms / s.delay_stats.count_pos as f64
                    )
                } else {
                    "—".to_string()
                },
                Style::default().fg(Color::White),
            ),
        ]),
        Line::from(vec![
            Span::raw("  负延迟(Poly先): "),
            Span::styled(
                format!("n={} ", s.delay_stats.count_neg),
                Style::default().fg(Color::Cyan),
            ),
            Span::raw("min="),
            Span::styled(
                if s.delay_stats.count_neg > 0 {
                    format!("{:.0}ms", s.delay_stats.min_neg_ms)
                } else {
                    "—".to_string()
                },
                Style::default().fg(Color::White),
            ),
            Span::raw(" max="),
            Span::styled(
                if s.delay_stats.count_neg > 0 {
                    format!("{:.0}ms", s.delay_stats.max_neg_ms)
                } else {
                    "—".to_string()
                },
                Style::default().fg(Color::White),
            ),
            Span::raw(" avg="),
            Span::styled(
                if s.delay_stats.count_neg > 0 {
                    format!(
                        "{:.0}ms",
                        s.delay_stats.sum_neg_ms / s.delay_stats.count_neg as f64
                    )
                } else {
                    "—".to_string()
                },
                Style::default().fg(Color::White),
            ),
        ]),
    ]);

    let para = Paragraph::new(text);
    frame.render_widget(para, inner);
}

/// 仓位面板：UP/DOWN 数量、均价、浮盈浮亏、均价之和、总盈亏扣费、追涨侧
fn render_position_panel(frame: &mut Frame, s: &AppState, area: ratatui::layout::Rect) {
    let block = Block::default()
        .title(" 📊 仓位 ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Magenta));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let up_mid = s.poly_up_mid();
    let down_mid = s.poly_down_mid();
    let float_up = s.ledger.position_up.float_pnl(up_mid);
    let float_down = s.ledger.position_down.float_pnl(down_mid);
    let chase_str = match s.chase_side {
        Some(ChaseSide::Up) => "UP",
        Some(ChaseSide::Down) => "DOWN",
        None => "—",
    };

    let text = Text::from(vec![
        Line::from(vec![
            Span::raw("  UP:  "),
            Span::styled(
                format!("Q={:.2}", s.ledger.position_up.qty),
                Style::default().fg(Color::White),
            ),
            Span::raw("  P_avg="),
            Span::styled(
                format!("{:.2}", s.ledger.position_up.avg_price),
                Style::default().fg(Color::Cyan),
            ),
            Span::raw("  浮盈/亏="),
            Span::styled(
                format!("{:.4}", float_up),
                Style::default().fg(if float_up >= 0.0 {
                    Color::Green
                } else {
                    Color::Red
                }),
            ),
        ]),
        Line::from(vec![
            Span::raw("  DOWN: "),
            Span::styled(
                format!("Q={:.2}", s.ledger.position_down.qty),
                Style::default().fg(Color::White),
            ),
            Span::raw("  P_avg="),
            Span::styled(
                format!("{:.2}", s.ledger.position_down.avg_price),
                Style::default().fg(Color::Cyan),
            ),
            Span::raw("  浮盈/亏="),
            Span::styled(
                format!("{:.4}", float_down),
                Style::default().fg(if float_down >= 0.0 {
                    Color::Green
                } else {
                    Color::Red
                }),
            ),
        ]),
        Line::from(vec![
            Span::raw("  均价之和="),
            Span::styled(
                format!("{:.2}", s.position_avg_sum()),
                Style::default().fg(if s.position_avg_sum() < 1.0 {
                    Color::Green
                } else {
                    Color::Yellow
                }),
            ),
            Span::raw("  挂单若成交="),
            Span::styled(
                format!("{:.2}", s.projected_avg_sum_after_intents()),
                Style::default().fg(if s.projected_avg_sum_after_intents() < 1.0 {
                    Color::Green
                } else {
                    Color::Yellow
                }),
            ),
            Span::raw(" (应<1)  偏仓="),
            Span::styled(
                {
                    let pu = s.projected_qty_up_after_intents();
                    let pd = s.projected_qty_down_after_intents();
                    if pu > pd {
                        "UP多"
                    } else if pd > pu {
                        "DOWN多"
                    } else {
                        "—"
                    }
                },
                Style::default().fg(Color::Cyan),
            ),
        ]),
        // v0.4.0-5m P&L 面板（merge 模型）
        Line::from(vec![
            Span::raw("  💰 现金 cash="),
            Span::styled(
                format!("{:+.4}", s.cash_pnl()),
                Style::default().fg(if s.cash_pnl() >= 0.0 {
                    Color::Green
                } else {
                    Color::Red
                }),
            ),
            Span::raw(" (paid -"),
            Span::styled(
                format!("{:.2}", s.ledger.cash_paid),
                Style::default().fg(Color::Red),
            ),
            Span::raw(", received +"),
            Span::styled(
                format!("{:.2}", s.ledger.cash_received),
                Style::default().fg(Color::Green),
            ),
            Span::raw(")"),
        ]),
        Line::from(vec![
            Span::raw("  📦 库存 inventory="),
            Span::styled(
                format!("{:+.4}", s.inventory_value()),
                Style::default().fg(Color::Cyan),
            ),
            Span::raw("  Taker费=-"),
            Span::styled(
                format!("{:.4}", s.ledger.total_fee),
                Style::default().fg(Color::Red),
            ),
            Span::raw("  Maker返佣=+"),
            Span::styled(
                format!("{:.4}", s.ledger.total_rebate),
                Style::default().fg(Color::Green),
            ),
        ]),
        Line::from(vec![
            Span::raw("  🔀 已 merge 对数="),
            Span::styled(
                format!("{:.2}", s.ledger.merged_pairs),
                Style::default().fg(Color::Magenta),
            ),
            Span::raw("  merge 实现盈亏="),
            Span::styled(
                format!("{:+.4}", s.ledger.merge_pnl),
                Style::default().fg(if s.ledger.merge_pnl >= 0.0 {
                    Color::Green
                } else {
                    Color::Red
                }),
            ),
            Span::raw("  可 merge 对数="),
            Span::styled(
                format!("{:.2}", s.mergeable_pairs()),
                Style::default().fg(Color::Yellow),
            ),
        ]),
        Line::from(vec![
            Span::raw("  📊 累计净盈亏="),
            Span::styled(
                format!("{:+.4} USDC", s.net_pnl()),
                Style::default()
                    .fg(if s.net_pnl() >= 0.0 {
                        Color::Green
                    } else {
                        Color::Red
                    })
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  (全周期 cash+inv-fee+返, redeem 已计入)  追涨侧="),
            Span::styled(chase_str, Style::default().fg(Color::Yellow)),
        ]),
        Line::from({
            let up_str = s
                .ledger
                .maker_buy_intent_up
                .as_ref()
                .map(|o| {
                    format!(
                        "UP @ {:.2}×{:.0}/{}",
                        o.price,
                        o.remaining_qty(),
                        pending_reason_label(o.reason)
                    )
                })
                .unwrap_or_else(|| "—".to_string());
            let down_str = s
                .ledger
                .maker_buy_intent_down
                .as_ref()
                .map(|o| {
                    format!(
                        "DOWN @ {:.2}×{:.0}/{}",
                        o.price,
                        o.remaining_qty(),
                        pending_reason_label(o.reason)
                    )
                })
                .unwrap_or_else(|| "—".to_string());
            vec![
                Span::raw("  挂单 "),
                Span::styled("Maker买", Style::default().fg(Color::Cyan)),
                Span::raw(": "),
                Span::styled(up_str, Style::default().fg(Color::White)),
                Span::raw("  "),
                Span::styled(down_str, Style::default().fg(Color::White)),
            ]
        }),
        Line::from({
            let up_hint = s
                .ledger
                .rebalance_hint_up
                .map(|(qty, avg)| format!("UP需{:.0}张 若成交均价和={:.2}", qty, avg))
                .unwrap_or_else(|| "—".to_string());
            let down_hint = s
                .ledger
                .rebalance_hint_down
                .map(|(qty, avg)| format!("DOWN需{:.0}张 若成交均价和={:.2}", qty, avg))
                .unwrap_or_else(|| "—".to_string());
            vec![
                Span::raw("  配平 "),
                Span::styled(up_hint, Style::default().fg(Color::DarkGray)),
                Span::raw("  "),
                Span::styled(down_hint, Style::default().fg(Color::DarkGray)),
            ]
        }),
    ]);

    let para = Paragraph::new(text);
    frame.render_widget(para, inner);
}

/// 状态栏
fn render_statusbar(frame: &mut Frame, s: &AppState, area: ratatui::layout::Rect) {
    let content = if let Some(warn_ms) = s.last_warn_ms {
        Line::from(vec![
            Span::styled(
                "  ⚠️  延迟告警: ",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:.1}ms 超过阈值", warn_ms),
                Style::default().fg(Color::Yellow),
            ),
            Span::styled("  |  按 'q' 退出", Style::default().fg(Color::DarkGray)),
        ])
    } else {
        Line::from(vec![
            Span::styled("  ✅ 运行正常  ", Style::default().fg(Color::Green)),
            Span::styled("|  按 'q' 退出", Style::default().fg(Color::DarkGray)),
        ])
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::DarkGray));

    let para = Paragraph::new(content).block(block);
    frame.render_widget(para, area);
}
