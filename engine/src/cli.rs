//! 启动参数解析。
//!
//! 默认走 TUI；`--web` 启用浏览器面板（axum + SSE）。
//! HEADLESS=1 环境变量仍然在两种模式之前生效，行为不变。

use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(
    name = "rust_engine",
    version,
    about = "Binance + Polymarket 5m 做市引擎"
)]
pub struct Cli {
    /// 启用 TUI 前端（默认；与 --web 互斥）
    #[arg(long, conflicts_with = "web")]
    pub tui: bool,

    /// 启用 Web 前端（浏览器访问，axum + SSE）
    #[arg(long)]
    pub web: bool,

    /// Web 服务器监听端口
    #[arg(long, default_value_t = 3000)]
    pub port: u16,

    /// Web 服务器监听地址
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// Trunk 构建产物目录（仅 --web 时使用）
    #[arg(long, default_value = "web-ui/dist")]
    pub web_dist: String,

    /// 强制 dry-run（即使配置了 [wallet].private_key 也忽略）
    #[arg(long, default_value_t = false)]
    pub dry_run: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Tui,
    Web,
}

impl Cli {
    pub fn mode(&self) -> Mode {
        if self.web {
            Mode::Web
        } else {
            Mode::Tui
        }
    }
}
