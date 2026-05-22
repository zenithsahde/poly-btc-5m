use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alloy_network::{EthereumWallet, TransactionBuilder};
use alloy_primitives::{address, keccak256, Address, Bytes, B256, U256};
use alloy_provider::{Provider, ProviderBuilder};
use alloy_rpc_types_eth::TransactionRequest;
use alloy_sol_types::{sol, SolCall};
use anyhow::{anyhow, Context, Result};
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::Client;
use serde_json::{json, Value};
use tracing::{debug, info, warn};

use crate::execution::signer::{PolySigner, SignatureMode};
use crate::execution::transaction::ApiCreds;

const CTF_ADDR: Address = address!("4D97DCd97eC945f40cF65F87097ACe5EA0476045");
const PUSD_ADDR: Address = address!("C011a7E12a19f7B1f670d46F03B03f3342E82DFB");
#[allow(dead_code)]
const DEPOSIT_WALLET_FACTORY: Address = address!("00000000000Fb5C9ADea0298D729A0CB3823Cc07");
const RELAYER_BASE: &str = "https://relayer-v2.polymarket.com";
const SUBMIT_PATH: &str = "/submit";
const POLYGON_CHAIN_ID: u64 = 137;
const MERGE_MIN_PAIRS: f64 = 0.000_001;
const MERGE_TIMEOUT: Duration = Duration::from_secs(60);
const MICRO: f64 = 1_000_000.0;

sol! {
    #[allow(missing_docs)]
    function mergePositions(
        address collateralToken,
        bytes32 parentCollectionId,
        bytes32 conditionId,
        uint256[] partition,
        uint256 amount
    );
}

fn pair_qty_to_amount_micro(pair_qty: f64) -> Option<(U256, f64)> {
    if !pair_qty.is_finite() || pair_qty < MERGE_MIN_PAIRS {
        return None;
    }
    let micro = (pair_qty * MICRO).floor() as u128;
    if micro == 0 {
        return None;
    }
    Some((U256::from(micro), micro as f64 / MICRO))
}

fn encode_merge_calldata(condition_id: B256, amount_micro: U256) -> Bytes {
    let call = mergePositionsCall {
        collateralToken: PUSD_ADDR,
        parentCollectionId: B256::ZERO,
        conditionId: condition_id,
        partition: vec![U256::from(1u64), U256::from(2u64)],
        amount: amount_micro,
    };
    call.abi_encode().into()
}

const SAFE_TX_TYPE_STR: &str = "SafeTx(address to,uint256 value,bytes data,uint8 operation,uint256 safeTxGas,uint256 baseGas,uint256 gasPrice,address gasToken,address refundReceiver,uint256 nonce)";
// Safe 1.3+ domain：仅 (chainId, verifyingContract)，无 name/version
const SAFE_DOMAIN_TYPE_STR: &str = "EIP712Domain(uint256 chainId,address verifyingContract)";

fn safe_domain_separator(safe_addr: Address) -> B256 {
    let type_hash = keccak256(SAFE_DOMAIN_TYPE_STR.as_bytes());
    let mut buf = [0u8; 96];
    buf[0..32].copy_from_slice(type_hash.as_slice());
    buf[32..64].copy_from_slice(&U256::from(POLYGON_CHAIN_ID).to_be_bytes::<32>());
    buf[76..96].copy_from_slice(safe_addr.as_slice());
    keccak256(buf)
}

fn build_safe_tx_digest(safe_addr: Address, inner_data: &Bytes, nonce: U256) -> B256 {
    let type_hash = keccak256(SAFE_TX_TYPE_STR.as_bytes());
    let data_hash = keccak256(inner_data.as_ref());
    let mut buf = [0u8; 32 * 11];
    buf[0..32].copy_from_slice(type_hash.as_slice());
    buf[44..64].copy_from_slice(CTF_ADDR.as_slice());
    buf[96..128].copy_from_slice(data_hash.as_slice());
    buf[32 * 10..32 * 11].copy_from_slice(&nonce.to_be_bytes::<32>());
    let struct_hash = keccak256(buf);

    let domain_sep = safe_domain_separator(safe_addr);
    let mut digest_input = [0u8; 66];
    digest_input[0] = 0x19;
    digest_input[1] = 0x01;
    digest_input[2..34].copy_from_slice(domain_sep.as_slice());
    digest_input[34..66].copy_from_slice(struct_hash.as_slice());
    keccak256(digest_input)
}

#[derive(Clone, Copy, Debug)]
enum NonceKind {
    Safe,
    #[allow(dead_code)]
    Wallet,
}

impl NonceKind {
    fn as_query(self) -> &'static str {
        match self {
            NonceKind::Safe => "SAFE",
            NonceKind::Wallet => "WALLET",
        }
    }
}

pub struct RelayerClient {
    http: Client,
    creds: ApiCreds,
    eoa_checksum: String,
}

impl RelayerClient {
    pub fn new(http: Client, creds: ApiCreds, eoa: Address) -> Self {
        Self {
            http,
            creds,
            eoa_checksum: eoa.to_checksum(None),
        }
    }

    fn stamp_headers(&self, method: &str, path: &str, body: &[u8]) -> Result<HeaderMap> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let sig = self
            .creds
            .sign_for_relayer(ts, method.as_bytes(), path.as_bytes(), body);
        let mut h = HeaderMap::with_capacity(8);
        h.insert("Accept", HeaderValue::from_static("*/*"));
        h.insert("Content-Type", HeaderValue::from_static("application/json"));
        h.insert("POLY_ADDRESS", HeaderValue::from_str(&self.eoa_checksum)?);
        h.insert("POLY_API_KEY", self.creds.api_key_header().clone());
        h.insert("POLY_PASSPHRASE", self.creds.passphrase_header().clone());
        h.insert("POLY_TIMESTAMP", HeaderValue::from_str(&ts.to_string())?);
        h.insert("POLY_SIGNATURE", HeaderValue::from_str(&sig)?);
        Ok(h)
    }

    async fn get_nonce(&self, kind: NonceKind) -> Result<U256> {
        let path = format!("/nonce?address={}&type={}", self.eoa_checksum, kind.as_query());
        let url = format!("{RELAYER_BASE}{path}");
        let headers = self.stamp_headers("GET", &path, b"")?;
        let resp = self
            .http
            .get(&url)
            .headers(headers)
            .send()
            .await
            .context("relayer GET /nonce 失败")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("relayer /nonce {status}: {text}");
        }
        parse_nonce(&text).with_context(|| format!("解析 nonce 失败: body={text}"))
    }

    // TODO: 若 relayer 实际异步返回 transactionID 而非 transactionHash，需补轮询
    async fn submit(&self, body: &Value) -> Result<B256> {
        let path = SUBMIT_PATH;
        let url = format!("{RELAYER_BASE}{path}");
        let body_vec = serde_json::to_vec(body)?;
        let headers = self.stamp_headers("POST", path, &body_vec)?;
        let resp = self
            .http
            .post(&url)
            .headers(headers)
            .body(body_vec)
            .send()
            .await
            .context("relayer POST /submit 失败")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!("relayer /submit {status}: {text}");
        }
        let json: Value = serde_json::from_str(&text)
            .with_context(|| format!("relayer /submit 响应非 JSON: {text}"))?;
        let tx_hex = json
            .get("transactionHash")
            .or_else(|| json.get("transaction_hash"))
            .or_else(|| json.get("hash"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("relayer /submit 缺少 transactionHash: {text}"))?;
        parse_b256(tx_hex).with_context(|| format!("非法 transactionHash: {tx_hex}"))
    }
}

fn parse_nonce(text: &str) -> Result<U256> {
    if let Ok(json) = serde_json::from_str::<Value>(text) {
        if let Some(s) = json.get("nonce").and_then(|v| v.as_str()) {
            return U256::from_str_radix(s, 10).map_err(|e| anyhow!("nonce 非十进制: {e}"));
        }
        if let Some(n) = json.get("nonce").and_then(|v| v.as_u64()) {
            return Ok(U256::from(n));
        }
        if let Some(s) = json.as_str() {
            return U256::from_str_radix(s, 10).map_err(|e| anyhow!("nonce 非十进制: {e}"));
        }
    }
    U256::from_str_radix(text.trim().trim_matches('"'), 10)
        .map_err(|e| anyhow!("nonce 解析失败: {e}"))
}

fn parse_b256(s: &str) -> Result<B256> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    let bytes = hex::decode(s).map_err(|e| anyhow!("hex decode: {e}"))?;
    if bytes.len() != 32 {
        anyhow::bail!("非 32 字节: {} bytes", bytes.len());
    }
    Ok(B256::from_slice(&bytes))
}

#[derive(Clone, Debug)]
pub struct MergeOutcome {
    pub pair_qty: f64,
    pub tx_hash: B256,
    pub elapsed_ms: u64,
}

enum MergeRoute {
    Eoa {
        wallet: EthereumWallet,
        rpc_url: String,
    },
    Safe {
        safe_addr: Address,
        relayer: Arc<RelayerClient>,
    },
    DepositWallet,
}

pub struct MergeClient {
    signer: Arc<PolySigner>,
    route: MergeRoute,
}

impl MergeClient {
    pub fn new_eoa(signer: Arc<PolySigner>, rpc_url: String) -> Self {
        let wallet = EthereumWallet::from(signer.local_signer());
        Self {
            signer,
            route: MergeRoute::Eoa { wallet, rpc_url },
        }
    }

    pub fn new_safe(signer: Arc<PolySigner>, relayer: Arc<RelayerClient>) -> Self {
        let safe_addr = signer.maker();
        Self {
            signer,
            route: MergeRoute::Safe { safe_addr, relayer },
        }
    }

    pub fn new_deposit_wallet(signer: Arc<PolySigner>) -> Self {
        Self {
            signer,
            route: MergeRoute::DepositWallet,
        }
    }

    pub async fn merge_pairs(&self, condition_id: B256, pair_qty: f64) -> Result<MergeOutcome> {
        let Some((amount_micro, normalized_pairs)) = pair_qty_to_amount_micro(pair_qty) else {
            anyhow::bail!("merge: pair_qty {} 低于阈值，跳过", pair_qty);
        };
        if condition_id == B256::ZERO {
            anyhow::bail!("merge: condition_id 为零，未注入");
        }
        let calldata = encode_merge_calldata(condition_id, amount_micro);
        let started = Instant::now();

        let tx_hash = match &self.route {
            MergeRoute::Eoa { wallet, rpc_url } => {
                send_eoa(wallet.clone(), rpc_url, calldata).await?
            }
            MergeRoute::Safe { safe_addr, relayer } => {
                send_safe(&self.signer, *safe_addr, relayer.as_ref(), calldata).await?
            }
            MergeRoute::DepositWallet => {
                anyhow::bail!(
                    "merge: DepositWallet 路径暂未实现，先用 signature_mode = \"eoa\" 或 \"safe\""
                );
            }
        };

        Ok(MergeOutcome {
            pair_qty: normalized_pairs,
            tx_hash,
            elapsed_ms: started.elapsed().as_millis() as u64,
        })
    }
}

async fn send_eoa(wallet: EthereumWallet, rpc_url: &str, calldata: Bytes) -> Result<B256> {
    let url = rpc_url
        .parse::<reqwest::Url>()
        .with_context(|| format!("非法 polygon_rpc_url: {rpc_url}"))?;
    let provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(wallet)
        .on_http(url);
    let tx = TransactionRequest::default()
        .with_to(CTF_ADDR)
        .with_input(calldata)
        .with_chain_id(POLYGON_CHAIN_ID);
    let pending = tokio::time::timeout(MERGE_TIMEOUT, provider.send_transaction(tx))
        .await
        .context("EOA merge: 发送交易超时")??;
    let tx_hash = *pending.tx_hash();
    info!(tx_hash = %tx_hash, "EOA merge: tx broadcast，等待回执…");
    let receipt = tokio::time::timeout(MERGE_TIMEOUT, pending.get_receipt())
        .await
        .with_context(|| format!("EOA merge: 等待回执超时 tx={tx_hash}"))??;
    if !receipt.status() {
        anyhow::bail!("EOA merge: tx reverted hash={tx_hash}");
    }
    debug!(tx_hash = %tx_hash, block = ?receipt.block_number, "EOA merge: 回执已确认");
    Ok(tx_hash)
}

async fn send_safe(
    signer: &PolySigner,
    safe_addr: Address,
    relayer: &RelayerClient,
    calldata: Bytes,
) -> Result<B256> {
    let nonce = relayer.get_nonce(NonceKind::Safe).await?;
    let digest = build_safe_tx_digest(safe_addr, &calldata, nonce);
    let sig = signer.sign_safe_personal(digest).await?;
    let signature_hex = format!("0x{}", hex::encode(sig));
    let data_hex = format!("0x{}", hex::encode(calldata.as_ref()));
    let body = json!({
        "type": "SAFE",
        "from": signer.signer_address().to_checksum(None),
        "to": CTF_ADDR.to_checksum(None),
        "proxyWallet": safe_addr.to_checksum(None),
        "data": data_hex,
        "nonce": nonce.to_string(),
        "signature": signature_hex,
        "signatureParams": {
            "operation": "0",
            "safeTxnGas": "0",
            "baseGas": "0",
            "gasPrice": "0",
            "gasToken": Address::ZERO.to_checksum(None),
            "refundReceiver": Address::ZERO.to_checksum(None),
        },
        "metadata": "merge",
    });
    let tx_hash = tokio::time::timeout(MERGE_TIMEOUT, relayer.submit(&body))
        .await
        .context("Safe merge: relayer 同步等待超时")??;
    info!(tx_hash = %tx_hash, safe = %safe_addr, "Safe merge: relayer 已返回 tx");
    Ok(tx_hash)
}

pub fn build_merge_client(
    signer: Arc<PolySigner>,
    creds: ApiCreds,
    rpc_url: &str,
    http: Client,
) -> Result<MergeClient> {
    match signer.signature_mode() {
        SignatureMode::Eoa => {
            if rpc_url.is_empty() {
                anyhow::bail!("EOA merge: polygon_rpc_url 未配置");
            }
            Ok(MergeClient::new_eoa(signer, rpc_url.to_string()))
        }
        SignatureMode::Safe => {
            let eoa = signer.signer_address();
            let relayer = Arc::new(RelayerClient::new(http, creds, eoa));
            Ok(MergeClient::new_safe(signer, relayer))
        }
        SignatureMode::DepositWallet => {
            warn!("merge: signature_mode=deposit_wallet 暂未实现，调用时会返回错误");
            Ok(MergeClient::new_deposit_wallet(signer))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calldata_selector_and_length() {
        let cid = B256::from([7u8; 32]);
        let data = encode_merge_calldata(cid, U256::from(1_000_000u64));
        assert_eq!(&data[..4], &mergePositionsCall::SELECTOR);
        assert_eq!(data.len(), 260);
    }

    #[test]
    fn pair_qty_below_threshold_is_skipped() {
        assert!(pair_qty_to_amount_micro(0.0).is_none());
        assert!(pair_qty_to_amount_micro(-1.0).is_none());
        assert!(pair_qty_to_amount_micro(f64::NAN).is_none());
        assert!(pair_qty_to_amount_micro(MERGE_MIN_PAIRS / 2.0).is_none());
    }

    #[test]
    fn pair_qty_above_threshold_rounds_down() {
        let (u, normalized) = pair_qty_to_amount_micro(1.234_567_89).unwrap();
        assert_eq!(u, U256::from(1_234_567u64));
        assert!((normalized - 1.234_567).abs() < 1e-9);
    }

    #[test]
    fn safe_tx_digest_is_deterministic() {
        let safe = address!("dddddddddddddddddddddddddddddddddddddddd");
        let data: Bytes = vec![0xde, 0xad, 0xbe, 0xef].into();
        let a = build_safe_tx_digest(safe, &data, U256::from(42u64));
        let b = build_safe_tx_digest(safe, &data, U256::from(42u64));
        assert_eq!(a, b);
        let c = build_safe_tx_digest(safe, &data, U256::from(43u64));
        assert_ne!(a, c);
        let safe2 = address!("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee");
        let d = build_safe_tx_digest(safe2, &data, U256::from(42u64));
        assert_ne!(a, d);
    }

    #[tokio::test]
    async fn safe_personal_sign_bumps_v_by_4() {
        let pk = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let signer = PolySigner::new(
            pk,
            SignatureMode::Safe,
            address!("dddddddddddddddddddddddddddddddddddddddd"),
            B256::ZERO,
        )
        .unwrap();
        let digest = B256::from([1u8; 32]);
        let sig = signer.sign_safe_personal(digest).await.unwrap();
        assert!(sig[64] == 31 || sig[64] == 32, "v={}, 期望 31 或 32", sig[64]);
    }
}
