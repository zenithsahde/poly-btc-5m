/// execution/gateway.rs - 自动化执行风控网关
/// 负责在下单前的最后几毫秒进行逻辑校验，拦截风险行为
use std::time::Instant;
use tracing::info;

pub struct ExecutionGateway {
    /// 单笔最大金额限制 (USDT)
    pub max_notional: f64,
    /// 累计库存上限 (Token 数量)
    pub max_inventory: f64,
    /// 冷却时间 (秒)
    pub cooldown_secs: f64,
    /// 滑点容忍度 (bps)
    pub slippage_tolerance_bps: f64,

    // 内部状态
    current_inventory: f64,
    last_execution_time: Option<Instant>,
}

impl ExecutionGateway {
    pub fn new() -> Self {
        Self {
            max_notional: 100.0,
            max_inventory: 1000.0,
            cooldown_secs: 1.0,
            slippage_tolerance_bps: 20.0,
            current_inventory: 0.0,
            last_execution_time: None,
        }
    }

    /// 执行预检：返回 Ok(()) 表示允许下单，Err(String) 返回拒绝原因
    pub fn check_can_fire(&mut self, price: f64, qty: f64, fair_price: f64) -> Result<(), String> {
        let notional = price * qty;

        // 1. 校验单笔金额
        if notional > self.max_notional {
            return Err(format!(
                "Reject: Notional {} > Limit {}",
                notional, self.max_notional
            ));
        }

        // 2. 校验库存水位 (此处仅为简化逻辑)
        if (self.current_inventory + qty).abs() > self.max_inventory {
            return Err(format!(
                "Reject: Inventory {} would exceed limit",
                self.current_inventory
            ));
        }

        // 3. 校验冷却时间
        if let Some(last) = self.last_execution_time {
            if last.elapsed().as_secs_f64() < self.cooldown_secs {
                return Err("Reject: Cooldown active".to_string());
            }
        }

        // 4. 校验滑点与公允价偏差
        // 如果是 Buy，价格必须低于 (Fair + Tolerance)
        // 此处简化处理：判断 price 对比 fair_price 的偏离程度
        let gap_bps = (price - fair_price).abs() / fair_price * 10000.0;
        if gap_bps > self.slippage_tolerance_bps {
            return Err(format!("Reject: Slippage too high ({:.1} bps)", gap_bps));
        }

        Ok(())
    }

    /// 确认下单后更新内部状态
    pub fn record_execution(&mut self, qty_change: f64) {
        self.current_inventory += qty_change;
        self.last_execution_time = Some(Instant::now());
        info!(
            "Execution Recorded: New Inventory = {}",
            self.current_inventory
        );
    }
}
