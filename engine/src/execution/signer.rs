/// execution/signer.rs - Polymarket EIP-712 签名模块
/// 基于 alloy-rs 实现高性能订单授权
use alloy_primitives::{address, Address};
use alloy_signer::Signer as AlloySigner;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::{sol, SolStruct};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::str::FromStr;

sol! {
    /// Polymarket CLOB 订单结构体 (EIP-712)
    #[derive(Debug, Serialize, Deserialize)]
    struct Order {
        uint256 salt;
        address maker;
        address signer;
        address taker;
        uint256 tokenId;
        uint256 makerAmount;
        uint256 takerAmount;
        uint256 expiration;
        uint256 nonce;
        uint256 feeRateBps;
        uint256 side; // 0=BUY, 1=SELL
        uint8 signatureType; // 0=EOA, 1=POLY_PROXY
    }

    /// Polymarket CLOB 撤单结构体 (EIP-712)
    #[derive(Debug, Serialize, Deserialize)]
    struct CancelOrder {
        uint256 orderHash;
    }
}

pub struct PolySigner {
    signer: PrivateKeySigner,
    domain: alloy_sol_types::Eip712Domain,
}

impl PolySigner {
    /// 初始化签名器
    pub fn new(priv_key: &str) -> Result<Self> {
        let signer = PrivateKeySigner::from_str(priv_key).context("无效的私钥格式")?;

        // 定义 Polymarket CTF Exchange Domain
        let domain = alloy_sol_types::eip712_domain! {
            name: "Polymarket CTF Exchange",
            version: "1",
            chain_id: 137, // Polygon Mainnet
            verifying_contract: address!("C5d7332C0d178D541C7d43ee0662C5F9707b08b7"), // 示例合约地址，需确认
        };

        Ok(Self { signer, domain })
    }

    /// 执行 EIP-712 签名并返回 65 字节签名结果
    pub async fn sign_order(&self, order: &Order) -> Result<Vec<u8>> {
        // 计算结构化哈希
        let hash = order.eip712_signing_hash(&self.domain);

        // 执行签名
        let sig = self.signer.sign_hash(&hash).await?;

        // 转换为 [r, s, v] 格式字节数组
        Ok(sig.as_bytes().to_vec())
    }

    /// 执行撤单签名
    pub async fn sign_cancel_order(&self, cancel: &CancelOrder) -> Result<Vec<u8>> {
        let hash = cancel.eip712_signing_hash(&self.domain);
        let sig = self.signer.sign_hash(&hash).await?;
        Ok(sig.as_bytes().to_vec())
    }

    /// 获取签名者地址
    pub fn address(&self) -> Address {
        self.signer.address()
    }
}
