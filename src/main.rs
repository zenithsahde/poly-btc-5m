// 初期开发阶段：让未使用字段/方法的 warning 不干扰 build 输出
#![allow(dead_code, unused_variables)]

/// main.rs - SJ Trading Engine 入口（TUI 版本）
///
/// 启动流程：
/// 1. 加载配置 + 初始化日志（写入文件，不干扰 TUI）
/// 2. 创建共享状态 Arc<RwLock<AppState>>
/// 3. 启动 WS 客户端 task（自动重连）
/// 4. 启动策略引擎 task（写入共享状态）
/// 5. 主线程运行 TUI 渲染循环（每 50ms 刷新）
/// 6. 按 'q' 或 Ctrl+C 优雅关闭
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::prelude::CrosstermBackend;

mod cli;
mod config;
mod execution;
mod metrics;
mod model;
mod position;
mod strategy;
mod tui;
mod web;
mod ws;

use alloy_primitives::Address;
use clap::Parser;
use cli::Cli;
use config::AppConfig;
use execution::balance::BalanceProvider;
use execution::client::OrderClient;
use execution::merge::build_merge_client;
use execution::signer::{parse_builder_code, PolySigner};
use execution::sim::ExecutionSim;
use execution::transaction::{build_http2_client, derive_api_key, ApiCreds, LiveOrderClient};
use std::str::FromStr;
use strategy::signal::SignalEngine;
use tracing::{error, info, warn};
use tui::app::AppState;
use ws::client::BinanceWsClient;
use ws::poly_client::PolyWsClient;

const MAIN_MAKER_TIMEOUT_MS: i64 = 5000;
const MAIN_SIM_SUBMIT_LATENCY_MS: i64 = 45;
const MAIN_SIM_CANCEL_LATENCY_MS: i64 = 45;

/// TUI 刷新率（毫秒）
const TUI_REFRESH_MS: u64 = 50; // 20 FPS

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // ── 1. 加载配置 ────────────────────────────────────────────
    let _ = dotenvy::dotenv();
    let mut cfg = AppConfig::load()?;

    // ── 1b. 提前创建共享状态（LiveOrderClient 需要 Arc<RwLock<AppState>> 写回 pending）
    let state = Arc::new(RwLock::new(AppState::new(&cfg.trading.symbol)));
    if let Ok(mut s) = state.write() {
        s.strike_price = cfg.trading.strike_price;
    }

    // ── 1b.1 SQLite writer：所有持久化（pnl_samples / my_orders / my_fills）走 mpsc，
    //        POST 热路径不阻塞 sqlite IO。dry-run 与实盘都启动，方便用同一套工具看 PnL。
    let db_tx = web::spawn_db_writer()?;

    // ── 1c. 区分 dry-run / 实盘模式，构造对应 OrderClient ────────
    let private_key = cfg
        .wallet
        .as_ref()
        .and_then(|w| w.private_key.as_deref())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let signature_mode = cfg
        .wallet
        .as_ref()
        .map(|w| w.signature_mode)
        .unwrap_or_default();
    let wallet_address = cfg
        .wallet
        .as_ref()
        .and_then(|w| w.wallet_address.as_deref())
        .and_then(|s| Address::from_str(s).ok())
        .unwrap_or(Address::ZERO);
    let builder_code_str = cfg
        .wallet
        .as_ref()
        .and_then(|w| w.builder_code.as_deref())
        .unwrap_or("");
    let builder_code = parse_builder_code(builder_code_str);
    let polygon_rpc_url = cfg
        .wallet
        .as_ref()
        .and_then(|w| w.polygon_rpc_url.as_deref())
        .unwrap_or("")
        .to_string();

    let order_client: Arc<dyn OrderClient> =
        if let (false, Some(pk)) = (cli.dry_run, private_key.as_deref()) {
            let signer = PolySigner::new(pk, signature_mode, wallet_address, builder_code)?;
            info!(
                "Live mode: type={:?} maker={:?} signer={:?}",
                signer.signature_type(),
                signer.maker(),
                signer.order_signer()
            );
            let http = build_http2_client()?;
            let (api_key, secret, passphrase) = derive_api_key(&signer, &http).await?;
            info!("derive-api-key ok (api_key={}…)", &api_key[..api_key.len().min(8)]);
            let creds = ApiCreds::new(&api_key, &secret, &passphrase)?;

            // User channel WS：订阅本账户所有市场的 order / trade 事件，
            // 经 user_ws_handler 回灌 ledger + my_orders / my_fills。
            let (user_ev_tx, user_ev_rx) = tokio::sync::mpsc::channel(256);
            let user_ws = Arc::new(ws::poly_user_ws::PolyUserWs::new(
                api_key, secret, passphrase, user_ev_tx,
            ));
            tokio::spawn(user_ws.run());
            tokio::spawn(execution::user_ws_handler::run_user_event_handler(
                user_ev_rx,
                Arc::clone(&state),
                db_tx.clone(),
            ));

            // 余额查询（失败仅日志，不阻塞启动）
            if polygon_rpc_url.is_empty() {
                warn!("[wallet].polygon_rpc_url 未配置，跳过 pUSD 余额查询");
            } else {
                match BalanceProvider::new(&polygon_rpc_url, signer.maker()) {
                    Ok(bp) => match bp.fetch_micro_usdc().await {
                        Ok(micro) => info!("pUSD balance: {:.4} USDC", micro as f64 / 1_000_000.0),
                        Err(e) => warn!("pUSD balance fetch failed: {:?}", e),
                    },
                    Err(e) => warn!("BalanceProvider init failed: {:?}", e),
                }
            }

            warn!("Live 模式：无 WS fill listener，GTC 挂单成交不会回流 PnL；先用小额 order_size_usdc 验。");

            // MergeClient 需要一份独立的 PolySigner（LiveOrderClient::with_http 消费 signer）
            let merge_signer = PolySigner::new(pk, signature_mode, wallet_address, builder_code)?;
            let merge_client = build_merge_client(
                Arc::new(merge_signer),
                creds.clone(),
                &polygon_rpc_url,
                http.clone(),
            )?;

            let mut live = LiveOrderClient::with_http(
                signer,
                creds,
                cfg.trading.order_size_usdc,
                MAIN_MAKER_TIMEOUT_MS,
                Arc::clone(&state),
                http,
                db_tx.clone(),
            )
            .await?;
            live.attach_merge_client(Arc::new(merge_client));

            // Resubmit 链：tx 注入 LiveOrderClient + worker spawn（self_tx 用于链式自送）
            let (resubmit_tx, resubmit_rx) =
                tokio::sync::mpsc::unbounded_channel::<execution::resubmit::ResubmitRequest>();
            live.attach_resubmit_tx(resubmit_tx.clone());
            let resub_shared = live.shared();
            tokio::spawn(execution::resubmit::run_resubmit_worker(
                resubmit_rx,
                resub_shared,
                resubmit_tx,
            ));

            if let Ok(mut s) = state.write() {
                s.is_live_mode = true;
            }
            Arc::new(live) as Arc<dyn OrderClient>
        } else {
            if cli.dry_run {
                info!("Dry-run mode forced by --dry-run flag");
            } else {
                warn!("[wallet].private_key not set — dry-run mode");
            }
            Arc::new(ExecutionSim::new(
                MAIN_SIM_SUBMIT_LATENCY_MS,
                MAIN_SIM_CANCEL_LATENCY_MS,
                MAIN_MAKER_TIMEOUT_MS,
            )) as Arc<dyn OrderClient>
        };
    if let Some(w) = cfg.wallet.as_mut() {
        w.private_key = None;
    }

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

    // ── 3. 共享状态已在 1b 创建 ────────────────────────────────

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
                    condition_id: alloy_primitives::B256::ZERO,
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
            condition_id: alloy_primitives::B256::ZERO,
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
        Arc::clone(&order_client),
    );
    let poly_task = tokio::spawn(async move {
        let _ = poly_client.run().await;
    });

    // ── 6. 启动策略引擎 task（OrderClient 接管下单/撤单）────────────
    let signal_engine = SignalEngine::new(cfg, Arc::clone(&state), Arc::clone(&order_client));
    let strategy_task = tokio::spawn(async move {
        signal_engine.run(event_rx).await;
    });

    // ── 6b. 启动 30ms 快照采集 task（v0.4.12：等间隔时序数据用于回测/优化）──
    let snap30_state = Arc::clone(&state);
    let snap30_task = tokio::spawn(async move {
        strategy::snap30ms::run_snapshot_task(snap30_state).await;
    });

    // ── 6c. 启动 30s PnL 采样 task：把 net_pnl / cash_pnl / inventory_value 落到 pnl_samples
    //        给 Web UI / 回测看（dry-run 也跑，用同一套工具）。
    {
        let pnl_state = Arc::clone(&state);
        let pnl_tx = db_tx.clone();
        tokio::spawn(async move {
            loop {
                let sample = match pnl_state.write() {
                    Ok(mut s) => s.record_web_pnl_sample(),
                    Err(p) => p.into_inner().record_web_pnl_sample(),
                };
                if let Some(point) = sample {
                    let _ = pnl_tx.send(web::DbMsg::PnlSample(point));
                }
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });
    }

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

    // ── 7. 初始化 TUI 终端 ─────────────────────────────────────
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;
    terminal.clear()?;

    // ── 8. TUI 主循环 ──────────────────────────────────────────
    let result = run_tui(&mut terminal, Arc::clone(&state)).await;

    // ── 9. 清理终端，恢复正常模式 ─────────────────────────────
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    ws_task.abort();
    poly_task.abort();
    strategy_task.abort();

    if let Err(e) = result {
        eprintln!("TUI 错误: {:?}", e);
    }

    println!("👋 SJ Trading Engine 已关闭");
    Ok(())
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
