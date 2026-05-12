use crate::execution::signer::{PolySigner, Order, CancelOrder};
use crate::execution::poly_api::PolyApiSubmitter;
use tokio::sync::mpsc;
use tracing::{info, error};

#[derive(Debug)]
pub enum SnipeCommand {
    /// 路线 A: Taker 下单 (狙击)
    Place { order: Order },
    /// 路线 B: Maker 撤单 (防守)
    Cancel { msg: CancelOrder },
}

pub struct ExecutionActor {
    signer: PolySigner,
    submitter: PolyApiSubmitter,
    rx: mpsc::Receiver<SnipeCommand>,
}

impl ExecutionActor {
    pub fn new(signer: PolySigner, rx: mpsc::Receiver<SnipeCommand>) -> Self {
        Self {
            signer,
            submitter: PolyApiSubmitter::new(),
            rx,
        }
    }

    pub async fn run(mut self) {
        info!("Execution Actor Started");
        while let Some(cmd) = self.rx.recv().await {
            match cmd {
                SnipeCommand::Place { order } => {
                    info!("🚀 Placing Order: tokenId={}", order.tokenId);
                    match self.signer.sign_order(&order).await {
                        Ok(sig) => {
                            let submitter = self.submitter.clone();
                            tokio::spawn(async move {
                                if let Err(e) = submitter.submit_order(order, sig).await {
                                    error!("Submit Order Failed: {:?}", e);
                                }
                            });
                        }
                        Err(e) => error!("Sign Order Failed: {:?}", e),
                    }
                }
                SnipeCommand::Cancel { msg } => {
                    info!("🛡️  Cancelling Order: hash={:?}", msg.orderHash);
                    match self.signer.sign_cancel_order(&msg).await {
                        Ok(sig) => {
                            let submitter = self.submitter.clone();
                            tokio::spawn(async move {
                                if let Err(e) = submitter.cancel_order(msg, sig).await {
                                    error!("Cancel Order Failed: {:?}", e);
                                }
                            });
                        }
                        Err(e) => error!("Sign Cancel Failed: {:?}", e),
                    }
                }
            }
        }
    }
}
