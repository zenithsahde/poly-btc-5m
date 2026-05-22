use alloy_primitives::{address, keccak256, Address, B256};
use alloy_signer::Signer as AlloySigner;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{sol, SolStruct};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

pub const EXCHANGE_V2: Address = address!("E111180000d2663C0091e4f400237545B87B996B");

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

pub fn parse_builder_code(code: Option<&str>) -> B256 {
    code.and_then(|s| {
        let s = s.strip_prefix("0x").unwrap_or(s);
        let bytes = hex::decode(s).ok()?;
        (bytes.len() == 32).then(|| B256::from_slice(&bytes))
    })
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
