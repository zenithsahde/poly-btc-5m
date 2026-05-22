/// ws/reconnect.rs - 自动重连策略
/// 指数退避 + Jitter，避免"雷群效应"
use std::time::Duration;

/// 重连策略参数
pub struct ReconnectPolicy {
    /// 基础等待时间
    base_ms: u64,
    /// 最大等待时间
    max_ms: u64,
    /// 当前尝试次数
    attempt: u32,
}

impl ReconnectPolicy {
    pub fn new(base_ms: u64, max_ms: u64) -> Self {
        Self {
            base_ms,
            max_ms,
            attempt: 0,
        }
    }

    /// 获取下一次等待时长（指数退避 + ±20% jitter）
    pub fn next_delay(&mut self) -> Duration {
        // 指数退避：base * 2^attempt
        let exp_ms = self.base_ms.saturating_mul(1u64 << self.attempt.min(10));
        let capped_ms = exp_ms.min(self.max_ms);

        // 注入 jitter：±20%
        let jitter_range = capped_ms / 5; // 20%
        let jitter = {
            // 简单随机（不引入 rand crate，用时间戳取模）
            let seed = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos() as u64;
            if seed % 2 == 0 {
                capped_ms.saturating_add(seed % jitter_range.max(1))
            } else {
                capped_ms.saturating_sub(seed % jitter_range.max(1))
            }
        };

        self.attempt += 1;
        Duration::from_millis(jitter)
    }

    /// 连接成功后重置计数
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// 当前尝试次数
    pub fn attempt(&self) -> u32 {
        self.attempt
    }
}
