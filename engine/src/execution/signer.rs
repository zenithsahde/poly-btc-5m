use alloy_primitives::{address, keccak256, Address, B256, U256};
use alloy_signer::Signer as AlloySigner;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{sol, SolStruct};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

use crate::execution::client::{OrderSide, OrderType, PlaceOrderRequest};

pub const EXCHANGE_V2: Address = address!("E111180000d2663C0091e4f400237545B87B996B");

// 金额精度（micro = 1e6 单位）。tick=0.01 → RoundConfig{price:2, size:2, amount:4}：
// size 档 = 2 位小数（micro 整除 10_000），amount 档 = 4 位小数（micro 整除 100）。
// BTC 5m up/down 常态 0.01 档，先硬编码；如遇 0.001 档市场需按真实 tick 调整。
const SIZE_STEP: u128 = 10_000; // 0.01 unit
const AMOUNT_STEP: u128 = 100; // 0.0001 unit

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignatureType {
    Eoa = 0,
    Safe = 2,
    DepositWallet = 3,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SignatureMode {
    Eoa,
    #[default]
    Safe,
    DepositWallet,
}

impl From<SignatureMode> for SignatureType {
    #[inline]
    fn from(m: SignatureMode) -> Self {
        match m {
            SignatureMode::Eoa => SignatureType::Eoa,
            SignatureMode::Safe => SignatureType::Safe,
            SignatureMode::DepositWallet => SignatureType::DepositWallet,
        }
    }
}

sol! {
    #[derive(Debug, Serialize, Deserialize)]
    struct Order {
        uint256 salt;
        address maker;
        address signer;
        uint256 tokenId;
        uint256 makerAmount;
        uint256 takerAmount;
        uint8   side;
        uint8   signatureType;
        uint256 timestamp;
        bytes32 metadata;
        bytes32 builder;
    }

    #[derive(Debug, Serialize, Deserialize)]
    struct CancelOrder {
        uint256 orderHash;
    }
}

#[inline]
fn floor_step(micro: u128, step: u128) -> u128 {
    (micro / step) * step
}

/// price(f64) → bps，按 0.01 tick 取整（0.53 → 5300）。
#[inline]
fn price_to_bps(price: f64) -> u128 {
    let cents = (price * 100.0).round().max(0.0) as u128;
    cents * 100
}

/// 计算 (maker_amount, taker_amount)，单位 micro(1e6)。
///
/// Polymarket 对市价(FAK/FOK)与限价(GTC/GTD)用不同精度规则（tick=0.01）：
/// - 限价 BUY：taker(shares) 取 2 位 → maker(USDC)=price×taker 取 4 位
/// - 市价 BUY：maker(USDC,花的钱) 取 2 位 → taker(shares)=maker/price 取 4 位
/// - SELL：maker(shares) 取 2 位 → taker(USDC)=price×maker 取 4 位（市/限同式）
/// 报错 "market buy ... maker max 2 decimals, taker max 4 decimals" 即市价 BUY 这条。
pub fn amounts_for(
    side: OrderSide,
    price: f64,
    size_shares: f64,
    order_type: OrderType,
) -> (u128, u128) {
    let bps = price_to_bps(price);
    if bps == 0 {
        return (0, 0);
    }
    let size_micro = (size_shares * 1_000_000.0).round().max(0.0) as u128;
    let is_market = matches!(order_type, OrderType::Fak);

    match side {
        OrderSide::Buy if is_market => {
            // maker(USDC,2dp) 为输入端；taker(shares)=maker/price 取 4dp
            let usdc_raw = bps * size_micro / 10_000;
            let maker = floor_step(usdc_raw, SIZE_STEP);
            let taker = floor_step(maker * 10_000 / bps, AMOUNT_STEP);
            (maker, taker)
        }
        OrderSide::Buy => {
            // 限价：taker(shares,2dp) 为输入端；maker(USDC)=price×taker 取 4dp
            let taker = floor_step(size_micro, SIZE_STEP);
            let maker = floor_step(bps * taker / 10_000, AMOUNT_STEP);
            (maker, taker)
        }
        OrderSide::Sell => {
            // maker(shares,2dp) 为输入端；taker(USDC)=price×maker 取 4dp
            let maker = floor_step(size_micro, SIZE_STEP);
            let taker = floor_step(bps * maker / 10_000, AMOUNT_STEP);
            (maker, taker)
        }
    }
}

pub fn parse_builder_code(code: &str) -> B256 {
    if code.is_empty() {
        return B256::ZERO;
    }
    let s = code.strip_prefix("0x").unwrap_or(code);
    hex::decode(s)
        .ok()
        .and_then(|bytes| (bytes.len() == 32).then(|| B256::from_slice(&bytes)))
        .unwrap_or(B256::ZERO)
}

pub struct PolySigner {
    key: PrivateKeySigner,
    mode: SignatureMode,
    wallet_address: Address,
    builder_code: B256,
    domain: alloy_sol_types::Eip712Domain,
}

impl PolySigner {
    pub fn new(
        priv_key: &str,
        mode: SignatureMode,
        wallet_address: Address,
        builder_code: B256,
    ) -> Result<Self> {
        let key = PrivateKeySigner::from_str(priv_key).context("无效的私钥格式")?;
        let domain = alloy_sol_types::eip712_domain! {
            name: "Polymarket CTF Exchange",
            version: "2",
            chain_id: 137_u64,
            verifying_contract: EXCHANGE_V2,
        };
        Ok(Self {
            key,
            mode,
            wallet_address,
            builder_code,
            domain,
        })
    }

    #[inline]
    pub fn signature_type(&self) -> SignatureType {
        SignatureType::from(self.mode)
    }

    #[inline]
    pub fn signature_mode(&self) -> SignatureMode {
        self.mode
    }

    #[inline]
    pub fn signer_address(&self) -> Address {
        self.key.address()
    }

    #[inline]
    pub fn maker(&self) -> Address {
        match self.mode {
            SignatureMode::Eoa => self.signer_address(),
            SignatureMode::Safe | SignatureMode::DepositWallet => self.wallet_address,
        }
    }

    #[inline]
    pub fn order_signer(&self) -> Address {
        match self.mode {
            SignatureMode::DepositWallet => self.maker(),
            _ => self.signer_address(),
        }
    }

    #[inline]
    pub fn builder(&self) -> B256 {
        self.builder_code
    }

    #[inline]
    pub fn local_signer(&self) -> PrivateKeySigner {
        self.key.clone()
    }

    // personal_sign(digest) + v+=4：Safe 1.3+ checkSignatures 的 eth_sign 分支
    pub async fn sign_safe_personal(&self, digest: B256) -> Result<[u8; 65]> {
        let sig = self.key.sign_message(digest.as_slice()).await?;
        let bytes = sig.as_bytes();
        let base_v = if bytes[64] < 27 { bytes[64] + 27 } else { bytes[64] };
        let mut out = [0u8; 65];
        out[..64].copy_from_slice(&bytes[..64]);
        out[64] = base_v + 4;
        Ok(out)
    }

    pub fn build_order(&self, req: &PlaceOrderRequest, salt: u64, ts_ms: u64) -> Order {
        let token_id = U256::from_str_radix(&req.token_id, 10).unwrap_or(U256::ZERO);
        let (maker_micro, taker_micro) =
            amounts_for(req.side, req.price, req.size_shares, req.order_type);
        Order {
            salt: U256::from(salt),
            maker: self.maker(),
            signer: self.order_signer(),
            tokenId: token_id,
            makerAmount: U256::from(maker_micro),
            takerAmount: U256::from(taker_micro),
            side: match req.side {
                OrderSide::Buy => 0,
                OrderSide::Sell => 1,
            },
            signatureType: self.signature_type() as u8,
            timestamp: U256::from(ts_ms),
            metadata: B256::ZERO,
            builder: self.builder_code,
        }
    }

    pub async fn sign_clob_auth(&self, timestamp_secs: u64, nonce: u64) -> Result<[u8; 65]> {
        let digest = clob_auth_digest(self.signer_address(), timestamp_secs, nonce);
        let sig = self.key.sign_hash(&digest).await?;
        let bytes = sig.as_bytes();
        let v = if bytes[64] < 27 { bytes[64] + 27 } else { bytes[64] };
        let mut out = [0u8; 65];
        out[..64].copy_from_slice(&bytes[..64]);
        out[64] = v;
        Ok(out)
    }

    pub async fn sign_order(&self, order: &Order) -> Result<Vec<u8>> {
        match self.mode {
            SignatureMode::Eoa | SignatureMode::Safe => {
                let hash = order.eip712_signing_hash(&self.domain);
                self.sign_legacy65(hash).await
            }
            SignatureMode::DepositWallet => self.sign_deposit_compound(order).await,
        }
    }

    pub async fn sign_cancel_order(&self, cancel: &CancelOrder) -> Result<Vec<u8>> {
        let hash = cancel.eip712_signing_hash(&self.domain);
        self.sign_legacy65(hash).await
    }

    async fn sign_legacy65(&self, hash: B256) -> Result<Vec<u8>> {
        let sig = self.key.sign_hash(&hash).await?;
        let bytes = sig.as_bytes();
        // Polymarket 要求 legacy v ∈ {27, 28}；alloy 输出 parity 0/1
        let v = if bytes[64] < 27 { bytes[64] + 27 } else { bytes[64] };
        let mut out = Vec::with_capacity(65);
        out.extend_from_slice(&bytes[..64]);
        out.push(v);
        Ok(out)
    }

    async fn sign_deposit_compound(&self, order: &Order) -> Result<Vec<u8>> {
        let deposit_wallet = order.maker;
        let contents_hash = compute_contents_hash(order, deposit_wallet);
        let app_domain_sep = exchange_v2_domain_separator();
        let tds_struct_hash = compute_tds_struct_hash(contents_hash, deposit_wallet);

        let mut digest_input = [0u8; 66];
        digest_input[0] = 0x19;
        digest_input[1] = 0x01;
        digest_input[2..34].copy_from_slice(app_domain_sep.as_slice());
        digest_input[34..66].copy_from_slice(tds_struct_hash.as_slice());
        let digest = keccak256(digest_input);

        let inner = self.key.sign_hash(&digest).await?;
        let inner_bytes = inner.as_bytes();
        let v = if inner_bytes[64] < 27 {
            inner_bytes[64] + 27
        } else {
            inner_bytes[64]
        };

        let type_bytes = ORDER_TYPE_STR.as_bytes();
        let ct_len = type_bytes.len() as u16;
        let mut compound = Vec::with_capacity(65 + 32 + 32 + type_bytes.len() + 2);
        compound.extend_from_slice(&inner_bytes[..64]);
        compound.push(v);
        compound.extend_from_slice(app_domain_sep.as_slice());
        compound.extend_from_slice(contents_hash.as_slice());
        compound.extend_from_slice(type_bytes);
        compound.extend_from_slice(&ct_len.to_be_bytes());
        Ok(compound)
    }
}

const ORDER_TYPE_STR: &str = "Order(uint256 salt,address maker,address signer,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint8 side,uint8 signatureType,uint256 timestamp,bytes32 metadata,bytes32 builder)";
const TYPED_DATA_SIGN_TYPE_STR: &str = "TypedDataSign(Order contents,string name,string version,uint256 chainId,address verifyingContract,bytes32 salt)Order(uint256 salt,address maker,address signer,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint8 side,uint8 signatureType,uint256 timestamp,bytes32 metadata,bytes32 builder)";

fn compute_contents_hash(order: &Order, deposit_wallet: Address) -> B256 {
    let order_type_hash = keccak256(ORDER_TYPE_STR.as_bytes());
    let mut buf = [0u8; 384];
    buf[0..32].copy_from_slice(order_type_hash.as_slice());
    buf[32..64].copy_from_slice(&order.salt.to_be_bytes::<32>());
    buf[76..96].copy_from_slice(order.maker.as_slice());
    buf[108..128].copy_from_slice(deposit_wallet.as_slice());
    buf[128..160].copy_from_slice(&order.tokenId.to_be_bytes::<32>());
    buf[160..192].copy_from_slice(&order.makerAmount.to_be_bytes::<32>());
    buf[192..224].copy_from_slice(&order.takerAmount.to_be_bytes::<32>());
    buf[255] = order.side;
    buf[287] = order.signatureType;
    buf[288..320].copy_from_slice(&order.timestamp.to_be_bytes::<32>());
    buf[320..352].copy_from_slice(order.metadata.as_slice());
    buf[352..384].copy_from_slice(order.builder.as_slice());
    keccak256(buf)
}

fn exchange_v2_domain_separator() -> B256 {
    let domain_type_hash = keccak256(
        b"EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)",
    );
    let mut buf = [0u8; 160];
    buf[0..32].copy_from_slice(domain_type_hash.as_slice());
    buf[32..64].copy_from_slice(keccak256(b"Polymarket CTF Exchange").as_slice());
    buf[64..96].copy_from_slice(keccak256(b"2").as_slice());
    buf[124..128].copy_from_slice(&137u32.to_be_bytes());
    buf[140..160].copy_from_slice(EXCHANGE_V2.as_slice());
    keccak256(buf)
}

// ClobAuth EIP-712 digest: domain={"ClobAuthDomain","1",137}, struct=ClobAuth(address,string,uint256,string)
fn clob_auth_digest(address: Address, timestamp_secs: u64, nonce: u64) -> B256 {
    const MESSAGE: &str = "This message attests that I control the given wallet";
    let type_hash = keccak256(
        b"ClobAuth(address address,string timestamp,uint256 nonce,string message)",
    );
    let ts_str = timestamp_secs.to_string();
    let mut struct_buf = [0u8; 160];
    struct_buf[0..32].copy_from_slice(type_hash.as_slice());
    struct_buf[44..64].copy_from_slice(address.as_slice());
    struct_buf[64..96].copy_from_slice(keccak256(ts_str.as_bytes()).as_slice());
    struct_buf[96..128].copy_from_slice(&U256::from(nonce).to_be_bytes::<32>());
    struct_buf[128..160].copy_from_slice(keccak256(MESSAGE.as_bytes()).as_slice());
    let struct_hash = keccak256(struct_buf);

    // Domain（无 verifyingContract，签名 type 含 4 字段而非 5）：
    // EIP712Domain(string name,string version,uint256 chainId)
    let domain_type_hash =
        keccak256(b"EIP712Domain(string name,string version,uint256 chainId)");
    let mut dom_buf = [0u8; 128];
    dom_buf[0..32].copy_from_slice(domain_type_hash.as_slice());
    dom_buf[32..64].copy_from_slice(keccak256(b"ClobAuthDomain").as_slice());
    dom_buf[64..96].copy_from_slice(keccak256(b"1").as_slice());
    dom_buf[124..128].copy_from_slice(&137u32.to_be_bytes());
    let domain_sep = keccak256(dom_buf);

    let mut digest_input = [0u8; 66];
    digest_input[0] = 0x19;
    digest_input[1] = 0x01;
    digest_input[2..34].copy_from_slice(domain_sep.as_slice());
    digest_input[34..66].copy_from_slice(struct_hash.as_slice());
    keccak256(digest_input)
}

fn compute_tds_struct_hash(contents_hash: B256, deposit_wallet: Address) -> B256 {
    let tds_type_hash = keccak256(TYPED_DATA_SIGN_TYPE_STR.as_bytes());
    let mut buf = [0u8; 224];
    buf[0..32].copy_from_slice(tds_type_hash.as_slice());
    buf[32..64].copy_from_slice(contents_hash.as_slice());
    buf[64..96].copy_from_slice(keccak256(b"DepositWallet").as_slice());
    buf[96..128].copy_from_slice(keccak256(b"1").as_slice());
    buf[156..160].copy_from_slice(&137u32.to_be_bytes());
    buf[172..192].copy_from_slice(deposit_wallet.as_slice());
    keccak256(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{PrimitiveSignature, U256};

    // 复现报错场景：order_size_usdc=10 / price=0.53 → size_shares≈18.8679 的市价 BUY。
    // 市价 BUY：maker(USDC) 必须 2dp（micro 整除 10_000），taker(shares) 4dp（整除 100）。
    #[test]
    fn market_buy_respects_2dp_maker_4dp_taker() {
        let size = 10.0 / 0.53; // 18.8679…
        let (maker, taker) = amounts_for(OrderSide::Buy, 0.53, size, OrderType::Fak);
        assert_eq!(maker % SIZE_STEP, 0, "maker 须 2dp(整除 10_000): {maker}");
        assert_eq!(taker % AMOUNT_STEP, 0, "taker 须 4dp(整除 100): {taker}");
        assert_eq!(maker, 10_000_000); // 10.00 USDC
        assert_eq!(taker, 18_867_900); // 18.8679 shares
    }

    // 限价 BUY：taker(shares) 2dp，maker(USDC) 4dp。干净输入应原样通过。
    #[test]
    fn limit_buy_clean_amounts() {
        let (maker, taker) = amounts_for(OrderSide::Buy, 0.5, 10.0, OrderType::Gtc);
        assert_eq!(taker, 10_000_000); // 10.00 shares
        assert_eq!(maker, 5_000_000); // 5.0000 USDC
        assert_eq!(taker % SIZE_STEP, 0);
        assert_eq!(maker % AMOUNT_STEP, 0);
    }

    // 限价 BUY 带零头 size：taker 截到 2dp，maker=price×taker 截到 4dp。
    #[test]
    fn limit_buy_truncates_share_size() {
        let (maker, taker) = amounts_for(OrderSide::Buy, 0.99, 10.005_678, OrderType::Gtc);
        assert_eq!(taker, 10_000_000); // 截到 2dp
        assert_eq!(maker % AMOUNT_STEP, 0);
        assert_eq!(maker, 9_900_000); // 0.99 × 10.00 = 9.9000
    }

    const TEST_KEY: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
    const EXPECTED_EOA: Address = address!("f39Fd6e51aad88F6F4ce6aB8827279cffFb92266");
    const TEST_WALLET: Address = address!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");

    fn sample_order(sig_type: SignatureType, maker: Address, signer: Address) -> Order {
        Order {
            salt: U256::from(0xC0FFEE_u64),
            maker,
            signer,
            tokenId: U256::from(1234567890_u64),
            makerAmount: U256::from(1_000_000_u64),
            takerAmount: U256::from(2_000_000_u64),
            side: 0,
            signatureType: sig_type as u8,
            timestamp: U256::from(1_700_000_000_000_u64),
            metadata: B256::ZERO,
            builder: B256::ZERO,
        }
    }

    #[tokio::test]
    async fn safe_path_signs_with_maker_eq_wallet_and_recovers_eoa() {
        let s = PolySigner::new(TEST_KEY, SignatureMode::Safe, TEST_WALLET, B256::ZERO).unwrap();
        assert_eq!(s.maker(), TEST_WALLET);
        assert_eq!(s.order_signer(), EXPECTED_EOA);

        let ord = sample_order(SignatureType::Safe, s.maker(), s.order_signer());
        let sig = s.sign_order(&ord).await.unwrap();
        assert_eq!(sig.len(), 65);
        assert!(matches!(sig[64], 27 | 28));

        let hash = ord.eip712_signing_hash(&s.domain);
        let recovered = PrimitiveSignature::from_raw(&sig)
            .unwrap()
            .recover_address_from_prehash(&hash)
            .unwrap();
        assert_eq!(recovered, EXPECTED_EOA);
    }

    #[tokio::test]
    async fn deposit_compound_layout_and_inner_sig_recovers_to_eoa() {
        let s = PolySigner::new(
            TEST_KEY,
            SignatureMode::DepositWallet,
            TEST_WALLET,
            B256::ZERO,
        )
        .unwrap();
        let ord = sample_order(SignatureType::DepositWallet, s.maker(), s.order_signer());
        let sig = s.sign_order(&ord).await.unwrap();

        let type_len = ORDER_TYPE_STR.len();
        assert_eq!(sig.len(), 65 + 32 + 32 + type_len + 2);
        assert!(matches!(sig[64], 27 | 28));

        let app_sep = exchange_v2_domain_separator();
        let contents = compute_contents_hash(&ord, ord.maker);
        assert_eq!(&sig[65..97], app_sep.as_slice());
        assert_eq!(&sig[97..129], contents.as_slice());
        assert_eq!(&sig[129..129 + type_len], ORDER_TYPE_STR.as_bytes());
        let len_be = u16::from_be_bytes([sig[129 + type_len], sig[129 + type_len + 1]]);
        assert_eq!(len_be as usize, type_len);

        let tds = compute_tds_struct_hash(contents, ord.maker);
        let mut digest_input = [0u8; 66];
        digest_input[0] = 0x19;
        digest_input[1] = 0x01;
        digest_input[2..34].copy_from_slice(app_sep.as_slice());
        digest_input[34..66].copy_from_slice(tds.as_slice());
        let digest = keccak256(digest_input);
        let recovered = PrimitiveSignature::from_raw(&sig[..65])
            .unwrap()
            .recover_address_from_prehash(&digest)
            .unwrap();
        assert_eq!(recovered, EXPECTED_EOA);
    }
}
