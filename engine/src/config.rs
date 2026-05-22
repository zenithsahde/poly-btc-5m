/// config.rs - 配置加载模块
/// 从 config/default.toml + 环境变量中加载配置
use anyhow::Result;
use config::{Config, Environment, File};
use serde::Deserialize;

#[derive(Debug, Deserialize, Clone)]
pub struct ExchangeConfig {
    pub ws_endpoint: String,
    pub ws_endpoint_alt: String,
    pub rest_endpoint: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TradingConfig {
    pub symbol: String,
    pub streams: Vec<String>,
    pub poly_token_id: String,
    pub wallet_address: String,
    /// 5m 公允价：行权价 K（未从 Poly 解析时使用）
    #[serde(default = "default_strike_price")]
    pub strike_price: f64,
    /// 5m 公允价：波动率默认值（样本不足时）
    #[serde(default = "default_volatility_annual")]
    pub default_volatility_annual: f64,
    #[serde(default = "default_sigma_min")]
    pub volatility_sigma_min: f64,
    #[serde(default = "default_sigma_max")]
    pub volatility_sigma_max: f64,
    /// 稳态下用 Poly 反解 IV 时的 σ 上限（允许更高以便 FV 贴近 Poly 盘口，例如 5.0）
    #[serde(default = "default_sigma_max_poly")]
    pub volatility_sigma_max_poly: f64,
}

fn default_strike_price() -> f64 {
    96000.0
}
fn default_volatility_annual() -> f64 {
    0.6
}
fn default_sigma_min() -> f64 {
    0.1
}
fn default_sigma_max() -> f64 {
    2.0
}
fn default_sigma_max_poly() -> f64 {
    5.0
}

#[derive(Debug, Deserialize, Clone)]
pub struct WebSocketConfig {
    pub ping_interval_secs: u64,
    pub pong_timeout_secs: u64,
    pub reconnect_base_ms: u64,
    pub reconnect_max_ms: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LatencyConfig {
    pub warn_threshold_ms: u64,
    pub report_interval_secs: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct OrderBookConfig {
    pub depth_levels: usize,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ApiConfig {
    pub api_key: String,
    pub secret_key: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct LoggingConfig {
    pub level: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AppConfig {
    pub exchange: ExchangeConfig,
    pub trading: TradingConfig,
    pub websocket: WebSocketConfig,
    pub latency: LatencyConfig,
    pub orderbook: OrderBookConfig,
    pub api: ApiConfig,
    pub logging: LoggingConfig,
}

impl AppConfig {
    pub fn load() -> Result<Self> {
        // 配置加载优先级：default.toml < 环境变量
        // 环境变量格式：APP__TRADING__SYMBOL=ETHUSDT（双下划线分隔层级）
        let config = Config::builder()
            .add_source(File::with_name("config/default"))
            .add_source(
                Environment::with_prefix("APP")
                    .separator("__")
                    .ignore_empty(true),
            )
            // API key 从独立环境变量读取（安全）
            .set_override_option("api.api_key", std::env::var("BINANCE_API_KEY").ok())?
            .set_override_option("api.secret_key", std::env::var("BINANCE_SECRET_KEY").ok())?
            .build()?;

        Ok(config.try_deserialize()?)
    }

    /// 构建 WebSocket 流地址
    /// 例：wss://stream.binance.com:9443/stream?streams=btcusdt@bookTicker/btcusdt@depth@100ms
    pub fn build_ws_url(&self) -> String {
        self.build_ws_url_for_endpoint(&self.exchange.ws_endpoint)
    }

    pub fn build_ws_url_alt(&self) -> String {
        self.build_ws_url_for_endpoint(&self.exchange.ws_endpoint_alt)
    }

    fn build_ws_url_for_endpoint(&self, endpoint: &str) -> String {
        let symbol_lower = self.trading.symbol.to_lowercase();
        let streams: Vec<String> = self
            .trading
            .streams
            .iter()
            .map(|s| format!("{}@{}", symbol_lower, s))
            .collect();
        let stream_path = streams.join("/");
        format!("{}/stream?streams={}", endpoint, stream_path)
    }
}
