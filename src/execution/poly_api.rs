use crate::execution::signer::{CancelOrder, Order};
use anyhow::Result;
use reqwest::Client;
use serde_json::json;
use tracing::info;

#[derive(Clone)]
pub struct PolyApiSubmitter {
    client: Client,
    base_url: String,
}

impl PolyApiSubmitter {
    pub fn new() -> Self {
        Self {
            client: Client::new(),
            base_url: "https://clob.polymarket.com".to_string(),
        }
    }

    pub async fn submit_order(&self, order: Order, signature: Vec<u8>) -> Result<String> {
        let url = format!("{}/order", self.base_url);
        let sig_hex = format!("0x{}", hex::encode(signature));

        let payload = json!({
            "order": {
                "salt":          order.salt.to_string(),
                "maker":         format!("{:?}", order.maker),
                "signer":        format!("{:?}", order.signer),
                "tokenId":       order.tokenId.to_string(),
                "makerAmount":   order.makerAmount.to_string(),
                "takerAmount":   order.takerAmount.to_string(),
                "side":          order.side,
                "signatureType": order.signatureType,
                "timestamp":     order.timestamp.to_string(),
                "metadata":      format!("0x{}", hex::encode(order.metadata)),
                "builder":       format!("0x{}", hex::encode(order.builder)),
            },
            "signature": sig_hex,
        });

        info!("Sending V2 Order to Poly: {}", url);
        let resp = self.client.post(url).json(&payload).send().await?;
        self.handle_response(resp).await
    }

    pub async fn cancel_order(&self, cancel: CancelOrder, signature: Vec<u8>) -> Result<String> {
        let url = format!("{}/order", self.base_url);
        let sig_hex = format!("0x{}", hex::encode(signature));

        let payload = json!({
            "order": { "orderHash": format!("{:?}", cancel.orderHash) },
            "signature": sig_hex,
        });

        info!("Cancelling Order on Poly: {:?}", cancel.orderHash);
        let resp = self.client.delete(url).json(&payload).send().await?;
        self.handle_response(resp).await
    }

    async fn handle_response(&self, resp: reqwest::Response) -> Result<String> {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if status.is_success() {
            Ok(body)
        } else {
            anyhow::bail!("Poly API Error ({}): {}", status, body);
        }
    }
}
