//! Web 前端服务（axum + SSE）。
//!
//! `--web` 模式下挂载本模块，提供：
//! - GET `/api/snapshot`     一次性 JSON 快照
//! - GET `/api/stream`       SSE 流，每 100ms 推送一帧 DashboardSnapshot
//! - GET `/`、`/assets/*`    Trunk 构建的静态资源

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
use tracing::{info, warn};

use crate::tui::app::AppState;

mod routes;
mod snapshot;
mod sse;

#[derive(Clone)]
pub struct WebState {
    pub app_state: Arc<RwLock<AppState>>,
}

pub async fn serve(
    app_state: Arc<RwLock<AppState>>,
    host: &str,
    port: u16,
    dist_dir: &str,
) -> Result<()> {
    let dist_path = PathBuf::from(dist_dir);
    if !dist_path.exists() {
        warn!(
            "Web dist 目录不存在: {} （将仅提供 /api 路由；请先在 web-ui/ 目录下运行 `trunk build`）",
            dist_path.display()
        );
    }

    let state = WebState { app_state };
    let app = routes::router(state, dist_path);

    let addr: SocketAddr = format!("{}:{}", host, port)
        .parse()
        .with_context(|| format!("无效的 host:port - {}:{}", host, port))?;

    info!("🌐 Web UI 监听 http://{}", addr);
    eprintln!("🌐 Web UI 已启动 → http://{}", addr);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("无法绑定 {}", addr))?;
    axum::serve(listener, app).await.context("axum::serve 失败")?;
    Ok(())
}
