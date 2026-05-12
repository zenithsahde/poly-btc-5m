//! axum 路由定义。

use std::path::{Path, PathBuf};

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

use super::WebState;

pub fn router(state: WebState, dist_dir: PathBuf) -> Router {
    let api = Router::new()
        .route("/api/snapshot", get(snapshot_handler))
        .route("/api/stream", get(stream_handler))
        .with_state(state);

    let mut app = api;

    // 静态资源（trunk 构建产物）。dist 不存在时仅返回 503 占位，方便先跑后端联调。
    if dist_dir.exists() {
        let index = dist_dir.join("index.html");
        if index.exists() {
            // Fallback 到 index.html 让 SPA 路由生效（同时单文件路由如 /favicon 由 ServeDir 处理）
            let serve_dir = ServeDir::new(&dist_dir).fallback(ServeFile::new(index));
            app = app.fallback_service(serve_dir);
        } else {
            app = app.fallback_service(ServeDir::new(&dist_dir));
        }
    } else {
        let dist_display = dist_dir.display().to_string();
        app = app.fallback(move || {
            let msg = dist_missing_message(&dist_display);
            async move { (StatusCode::SERVICE_UNAVAILABLE, msg).into_response() }
        });
    }

    app.layer(TraceLayer::new_for_http())
}

async fn snapshot_handler(State(s): State<WebState>) -> Json<shared_types::DashboardSnapshot> {
    Json(super::snapshot::build(&s.app_state))
}

async fn stream_handler(State(s): State<WebState>) -> impl IntoResponse {
    super::sse::dashboard_stream(s.app_state)
}

fn dist_missing_message(dist: &str) -> String {
    format!(
        "web-ui dist 目录不存在: {dist}\n\n请先构建前端：\n  cd web-ui && trunk build --release\n\n然后重新启动 `--web` 模式即可。",
    )
}

// 让 clippy 不警告未用导入
#[allow(dead_code)]
fn _path_unused(_p: &Path) {}
