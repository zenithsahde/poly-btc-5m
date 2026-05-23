//! 手动下单冒烟测试：复用 transaction.rs / signer.rs 的真实代码路径，POST 一笔 GTC 限价买单。
//!
//! 跑法（在仓库根目录，需先在 .env / env 里设好 POLY_PRIVATE_KEY，config.toml 里设好
//! [wallet] wallet_address / signature_mode / builder_code）：
//!
//!   cargo run --bin place_test_order
//!
//! 默认参数：token_id / shares=10 / price=0.5 / GTC。可用 env 覆盖：
//!   TEST_TOKEN_ID, TEST_SHARES, TEST_PRICE
//!
//! 会把最终请求 body 和服务端原始响应都打出来，方便确认还缺哪些字段。

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use reqwest::header::{HeaderMap, HeaderValue};

use rust_engine::config::AppConfig;
use rust_engine::execution::client::{OrderSide, OrderType, PlaceOrderRequest};
use rust_engine::execution::signer::{parse_builder_code, PolySigner};
use rust_engine::execution::transaction::{
    build_http2_client, derive_api_key, gen_salt, to_json_v2, ApiCreds,
};
use alloy_primitives::Address;
use std::str::FromStr;

const ORDER_URL: &str = "https://clob.polymarket.com/order";
const ORDER_PATH: &str = "/order";

const DEFAULT_TOKEN_ID: &str =
    "52391726616442101523613034369252498047772872673075270194315842695744840136100";
const DEFAULT_SHARES: f64 = 10.0;
const DEFAULT_PRICE: f64 = 0.5;

#[inline]
fn unix_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_target(false)
        .init();

    // ── 1. 读取参数 ─────────────────────────────────────────────
    let token_id = std::env::var("TEST_TOKEN_ID").unwrap_or_else(|_| DEFAULT_TOKEN_ID.to_string());
    let shares: f64 = std::env::var("TEST_SHARES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_SHARES);
    let price: f64 = std::env::var("TEST_PRICE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PRICE);

    // ── 2. 加载 wallet 配置（config.toml + env） ────────────────
    let cfg = AppConfig::load()?;
    let wallet = cfg
        .wallet
        .ok_or_else(|| anyhow!("config.toml 缺 [wallet] 段"))?;
    let pk = wallet
        .private_key
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("POLY_PRIVATE_KEY 未设置"))?;
    let wallet_address = wallet
        .wallet_address
        .as_deref()
        .and_then(|s| Address::from_str(s).ok())
        .unwrap_or(Address::ZERO);
    let builder_code = parse_builder_code(&wallet.builder_code);

    let signer = PolySigner::new(pk, wallet.signature_mode, wallet_address, builder_code)?;
    println!(
        "signer: type={:?} maker={} signer={}",
        signer.signature_type(),
        signer.maker().to_checksum(None),
        signer.order_signer().to_checksum(None),
    );

    // ── 3. 派生 CLOB API key ────────────────────────────────────
    let http = build_http2_client()?;
    let (api_key, secret, passphrase) = derive_api_key(&signer, &http).await?;
    let creds = ApiCreds::new(&api_key, &secret, &passphrase)?;
    println!("api_key: {}…", api_key.get(..8).unwrap_or(&api_key));

    // ── 4. 构造 + 签名订单 ──────────────────────────────────────
    let req = PlaceOrderRequest {
        side: OrderSide::Buy,
        token_id: token_id.clone(),
        price,
        size_shares: shares,
        order_type: OrderType::Gtc,
    };
    let salt: u64 = gen_salt();
    let ts_ms = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
    let order = signer.build_order(&req, salt, ts_ms);
    let sig = signer.sign_order(&order).await?;

    let body_json = to_json_v2(&order, &sig, OrderType::Gtc, creds.api_key());
    let body = serde_json::to_vec(&body_json)?;
    println!("\n=== request body ===\n{}\n", serde_json::to_string_pretty(&body_json)?);

    // ── 5. 组装鉴权头并 POST ────────────────────────────────────
    let ts = unix_ts();
    let l2_sig = creds.sign_for_relayer(ts, b"POST", ORDER_PATH.as_bytes(), &body);

    let mut headers = HeaderMap::new();
    headers.insert("Accept", HeaderValue::from_static("*/*"));
    headers.insert("Content-Type", HeaderValue::from_static("application/json"));
    // POLY_ADDRESS = API key 归属地址 = EOA(signer)，不是 Safe/maker。
    headers.insert(
        "POLY_ADDRESS",
        HeaderValue::from_str(&signer.signer_address().to_checksum(None))?,
    );
    headers.insert("POLY_API_KEY", creds.api_key_header().clone());
    headers.insert("POLY_PASSPHRASE", creds.passphrase_header().clone());
    headers.insert("POLY_TIMESTAMP", HeaderValue::from_str(&ts.to_string())?);
    headers.insert("POLY_SIGNATURE", HeaderValue::from_str(&l2_sig)?);

    let resp = http.post(ORDER_URL).headers(headers).body(body).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    println!("=== response ===\nstatus: {status}\nbody:   {text}");

    if status.is_success() {
        println!("\n✅ 下单成功");
    } else {
        println!("\n❌ 下单被拒：{status}");
    }
    Ok(())
}
