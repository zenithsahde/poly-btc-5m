// 初期开发阶段：让未使用字段/方法的 warning 不干扰 build 输出
#![allow(dead_code, unused_variables)]

/// main.rs - SJ Trading Engine 入口
///
/// 启动流程：
/// 1. 解析 CLI（默认 `--tui`，可选 `--web`）
/// 2. 加载配置 + 初始化日志
/// 3. 创建共享状态 Arc<RwLock<AppState>>
/// 4. 启动 WS 客户端 / 策略引擎 / 30ms 快照 tasks
/// 5. 按模式分发：
///    - `HEADLESS=1`：跳过 UI，仅采集
///    - `--tui`（默认）：终端 TUI 50ms 刷新
///    - `--web`：axum + SSE，浏览器面板
/// 6. 'q' / Ctrl+C 优雅关闭
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::CrosstermBackend;

mod cli;
mod config;
mod metrics;
mod model;
mod strategy;
mod tui;
mod web;
mod ws;

use cli::{Cli, Mode};
use config::AppConfig;
use strategy::signal::SignalEngine;
use tui::app::AppState;
use ws::client::BinanceWsClient;
use ws::poly_client::PolyWsClient;
use tracing::{info, error};

/// TUI 刷新率（毫秒）
const TUI_REFRESH_MS: u64 = 50; // 20 FPS

#[tokio::main]
async fn main() -> Result<()> {
    // ── 0. 解析 CLI ─────────────────────────────────────────────
    let cli = Cli::parse();

    // ── 1. 加载配置 ────────────────────────────────────────────
    let _ = dotenvy::dotenv();
    let cfg = AppConfig::load()?;

    // ── 2. 日志（避免与 TUI 冲突）──────────────────────────────
    // 若设置 RUST_LOG=info|debug，则开启日志。默认 stderr；若设置 ENGINE_LOG_FILE=路径 则写入该文件便于排查
    if std::env::var("RUST_LOG").is_ok() {
        let filter = tracing_subscriber::EnvFilter::from_default_env();
        if let Ok(path) = std::env::var("ENGINE_LOG_FILE") {
            match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                Ok(file) => {
                    tracing_subscriber::fmt()
                        .with_writer(std::sync::Mutex::new(file))
                        .with_env_filter(filter)
                        .compact()
                        .init();
                    info!("日志写入文件: {}", path);
                }
                Err(e) => {
                    eprintln!("ENGINE_LOG_FILE 打开失败 {}: {:?}", path, e);
                    tracing_subscriber::fmt()
                        .with_writer(std::io::stderr)
                        .with_env_filter(filter)
                        .compact()
                        .init();
                }
            }
        } else {
            tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_env_filter(filter)
                .compact()
                .init();
        }
    }

    // ── 3. 创建共享状态 ────────────────────────────────────────
    let state = Arc::new(RwLock::new(AppState::new(&cfg.trading.symbol)));
    if let Ok(mut s) = state.write() {
        s.strike_price = cfg.trading.strike_price;
    }

    // ── 4. 创建 WebSocket 客户端 ───────────────────────────────
    let (ws_client, event_rx) = BinanceWsClient::new(cfg.clone(), Arc::clone(&state));
    let event_tx = ws_client.event_tx_clone();

    // ── 5. 启动 WebSocket 客户端 task ─────────────────────────
    let ws_task = tokio::spawn(async move {
        let _ = ws_client.run().await;
    });

    // ── 5b. 启动 Polymarket WebSocket 客户端（仅 WS，自动发现 + 5m 自动切换）──
    let poly_initial = if cfg.trading.poly_token_id.is_empty() {
        info!("未指定 Poly Token ID，启用自动发现 5m 市场...");
        match ws::discovery::MarketDiscovery::get_active_5m_btc_market().await {
            Ok(m) => m,
            Err(e) => {
                error!("Polymarket 市场发现失败: {:?}", e);
                ws::discovery::Active5mMarket {
                    up_token_id: cfg.trading.poly_token_id.clone(),
                    down_token_id: String::new(),
                    slug: "unknown".to_string(),
                    window_end_ts: 0,
                }
            }
        }
    } else {
        let now = chrono::Utc::now().timestamp();
        let window_start = (now / 300) * 300;
        ws::discovery::Active5mMarket {
            up_token_id: cfg.trading.poly_token_id.clone(),
            down_token_id: String::new(),
            slug: format!("btc-updown-5m-{}", window_start),
            window_end_ts: window_start + 300,
        }
    };

    // 首屏 K：若有窗口则用币安该窗口开始时刻的开盘价，否则用配置默认
    if poly_initial.window_end_ts > 0 {
        let window_start = poly_initial.window_end_ts - 300;
        match ws::binance_rest::get_spot_price_at_time(
            &cfg.exchange.rest_endpoint,
            &cfg.trading.symbol,
            window_start,
        )
        .await
        {
            Ok(open) => {
                if let Ok(mut s) = state.write() {
                    s.strike_price = open.round();
                }
                info!("首屏 K 取自币安窗口开始秒内首笔成交: {:.0} (ts={})", open.round(), window_start);
            }
            Err(e) => tracing::warn!("首屏 K 拉取失败，使用配置: {:?}", e),
        }
    }

    let poly_client = PolyWsClient::new(
        poly_initial,
        event_tx,
        Arc::clone(&state),
        cfg.exchange.rest_endpoint.clone(),
        cfg.trading.symbol.clone(),
    );
    let poly_task = tokio::spawn(async move {
        let _ = poly_client.run().await;
    });

    // ── 6. 启动策略引擎 task（仅行情写入状态，无下单）────────────────
    let signal_engine = SignalEngine::new(cfg, Arc::clone(&state));
    let strategy_task = tokio::spawn(async move {
        signal_engine.run(event_rx).await;
    });

    // ── 6b. 启动 30ms 快照采集 task（v0.4.12：等间隔时序数据用于回测/优化）──
    let snap30_state = Arc::clone(&state);
    let snap30_task = tokio::spawn(async move {
        strategy::snap30ms::run_snapshot_task(snap30_state).await;
    });

    // ── 7. Headless 模式（HEADLESS=1 跳过 TUI，用于数据采集后台运行）──
    let headless = std::env::var("HEADLESS").map(|v| v == "1").unwrap_or(false);
    let run_secs: u64 = std::env::var("RUN_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    if headless {
        eprintln!("🟢 HEADLESS 模式：跳过 TUI，数据采集运行中...");
        if run_secs > 0 {
            eprintln!("    将在 {}s 后退出", run_secs);
            tokio::time::sleep(Duration::from_secs(run_secs)).await;
        } else {
            // 无超时：等待 Ctrl+C
            let _ = tokio::signal::ctrl_c().await;
        }
        ws_task.abort();
        poly_task.abort();
        strategy_task.abort();
        snap30_task.abort();
        eprintln!("👋 Headless 模式退出（CSV 已落盘）");
        return Ok(());
    }

    // ── 8. 模式分发 ───────────────────────────────────────────
    let result = match cli.mode() {
        Mode::Tui => run_tui_mode(Arc::clone(&state)).await,
        Mode::Web => {
            eprintln!(
                "🟢 Web 模式：访问 http://{}:{}/  （Ctrl+C 退出）",
                cli.host, cli.port
            );
            tokio::select! {
                r = web::serve(Arc::clone(&state), &cli.host, cli.port, &cli.web_dist) => r,
                _ = tokio::signal::ctrl_c() => {
                    eprintln!("\n👋 收到 Ctrl+C，关闭中...");
                    Ok(())
                }
            }
        }
    };

    // ── 9. 清理任务 ───────────────────────────────────────────
    ws_task.abort();
    poly_task.abort();
    strategy_task.abort();
    snap30_task.abort();

    if let Err(e) = result {
        eprintln!("运行错误: {:?}", e);
    }

    println!("👋 SJ Trading Engine 已关闭");
    Ok(())
}

/// TUI 模式：进入备用屏幕，运行 50ms 刷新循环，退出时清理终端。
async fn run_tui_mode(state: Arc<RwLock<AppState>>) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;
    terminal.clear()?;

    let result = run_tui(&mut terminal, state).await;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

/// TUI 渲染主循环
async fn run_tui(
    terminal: &mut ratatui::Terminal<CrosstermBackend<std::io::Stdout>>,
    state: Arc<RwLock<AppState>>,
) -> Result<()> {
    let tick = Duration::from_millis(TUI_REFRESH_MS);

    loop {
        // 渲染一帧
        terminal.draw(|frame| {
            tui::ui::render(frame, &state);
        })?;

        // 非阻塞检查键盘事件
        if event::poll(tick)? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
                            return Ok(());
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}
