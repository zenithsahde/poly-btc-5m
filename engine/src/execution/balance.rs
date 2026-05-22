use alloy_primitives::{address, Address, U256};
use alloy_provider::{ProviderBuilder, RootProvider};
use alloy_sol_types::sol;
use alloy_transport_http::Http;
use anyhow::{Context, Result};
use reqwest::Client as ReqwestClient;

// pUSD proxy on Polygon mainnet (chain 137).
pub const PUSD_ADDRESS: Address = address!("C011a7E12a19f7B1f670d46F03B03f3342E82DFB");

sol! {
    #[sol(rpc)]
    interface IERC20 {
        function balanceOf(address account) external view returns (uint256);
    }
}

#[inline]
pub fn u256_to_micro(raw: U256) -> u64 {
    u64::try_from(raw).unwrap_or(u64::MAX)
}

pub struct BalanceProvider {
    provider: RootProvider<Http<ReqwestClient>>,
    maker: Address,
}

impl BalanceProvider {
    pub fn new(polygon_rpc_url: &str, maker: Address) -> Result<Self> {
        let url = polygon_rpc_url
            .parse()
            .with_context(|| format!("非法 Polygon RPC URL: {polygon_rpc_url}"))?;
        let provider = ProviderBuilder::new().on_http(url);
        Ok(Self { provider, maker })
    }

    pub async fn fetch_micro_usdc(&self) -> Result<u64> {
        let erc20 = IERC20::new(PUSD_ADDRESS, &self.provider);
        let raw = erc20
            .balanceOf(self.maker)
            .call()
            .await
            .context("pUSD balanceOf 调用失败")?;
        Ok(u256_to_micro(raw._0))
    }
}
