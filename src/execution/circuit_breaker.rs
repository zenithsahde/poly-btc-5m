//! 安全熔断：连续下单失败 / 当日亏损超阈 → 永久 halt LiveOrderClient（含 resubmit），
//! 重启进程才解除。仅作用于实盘；dry-run / ExecutionSim 不持有 CircuitBreaker。

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::RwLock;

use serde::Deserialize;
use tracing::warn;

#[derive(Debug, Deserialize, Clone)]
pub struct CircuitBreakerConfig {
    #[serde(default = "default_cb_enabled")]
    pub enabled: bool,
    #[serde(default = "default_cb_max_daily_loss")]
    pub max_daily_loss: f64,
    #[serde(default = "default_cb_max_consecutive_errors")]
    pub max_consecutive_errors: u32,
}

fn default_cb_enabled() -> bool { true }
fn default_cb_max_daily_loss() -> f64 { 50.0 }
fn default_cb_max_consecutive_errors() -> u32 { 5 }

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            enabled: default_cb_enabled(),
            max_daily_loss: default_cb_max_daily_loss(),
            max_consecutive_errors: default_cb_max_consecutive_errors(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum TripReason {
    MaxDailyLoss { cash_pnl: f64, threshold: f64 },
    ConsecutiveErrors { count: u32, threshold: u32 },
}

impl fmt::Display for TripReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TripReason::MaxDailyLoss { cash_pnl, threshold } => write!(
                f,
                "MaxDailyLoss(cash_pnl={:.4} USDC, threshold=-{:.4})",
                cash_pnl, threshold
            ),
            TripReason::ConsecutiveErrors { count, threshold } => write!(
                f,
                "ConsecutiveErrors({}/{})",
                count, threshold
            ),
        }
    }
}

pub struct CircuitBreaker {
    config: CircuitBreakerConfig,
    halted: AtomicBool,
    trip_reason: RwLock<Option<TripReason>>,
    consecutive_errors: AtomicI64,
}

impl CircuitBreaker {
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            config,
            halted: AtomicBool::new(false),
            trip_reason: RwLock::new(None),
            consecutive_errors: AtomicI64::new(0),
        }
    }

    #[inline]
    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    #[inline]
    pub fn is_halted(&self) -> bool {
        if !self.config.enabled {
            return false;
        }
        self.halted.load(Ordering::Acquire)
    }

    pub fn trip_reason(&self) -> Option<TripReason> {
        self.trip_reason.read().ok().and_then(|g| g.clone())
    }

    /// 入口校验：已 halt → Err；否则按 cash_pnl 判定当日亏损是否触阈。
    pub fn check(&self, cash_pnl: f64) -> Result<(), TripReason> {
        if !self.config.enabled {
            return Ok(());
        }
        if self.halted.load(Ordering::Acquire) {
            return Err(self.trip_reason().unwrap_or(TripReason::MaxDailyLoss {
                cash_pnl,
                threshold: self.config.max_daily_loss,
            }));
        }
        if cash_pnl < -self.config.max_daily_loss {
            let reason = TripReason::MaxDailyLoss {
                cash_pnl,
                threshold: self.config.max_daily_loss,
            };
            self.trip(reason.clone());
            return Err(reason);
        }
        Ok(())
    }

    pub fn record_success(&self) {
        if !self.config.enabled {
            return;
        }
        self.consecutive_errors.store(0, Ordering::Release);
    }

    pub fn record_error(&self) {
        if !self.config.enabled {
            return;
        }
        let next = self.consecutive_errors.fetch_add(1, Ordering::AcqRel) + 1;
        if next as u32 >= self.config.max_consecutive_errors {
            self.trip(TripReason::ConsecutiveErrors {
                count: next as u32,
                threshold: self.config.max_consecutive_errors,
            });
        }
    }

    fn trip(&self, reason: TripReason) {
        // 第一次 trip 才记录原因 + 日志，避免后续重复刷屏。halted 状态永不复位。
        if self
            .halted
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            if let Ok(mut g) = self.trip_reason.write() {
                *g = Some(reason.clone());
            }
            warn!(%reason, "🛑 CircuitBreaker tripped — LiveOrderClient halted until process restart");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(enabled: bool, max_loss: f64, max_err: u32) -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            enabled,
            max_daily_loss: max_loss,
            max_consecutive_errors: max_err,
        }
    }

    #[test]
    fn consecutive_errors_trip() {
        let cb = CircuitBreaker::new(cfg(true, 100.0, 3));
        cb.record_error();
        cb.record_error();
        assert!(!cb.is_halted());
        cb.record_error();
        assert!(cb.is_halted());
        // halt 不可逆：check 必返回 Err
        assert!(cb.check(0.0).is_err());
    }

    #[test]
    fn success_resets_counter() {
        let cb = CircuitBreaker::new(cfg(true, 100.0, 3));
        cb.record_error();
        cb.record_error();
        cb.record_success();
        cb.record_error();
        cb.record_error();
        assert!(!cb.is_halted());
    }

    #[test]
    fn max_daily_loss_trip() {
        let cb = CircuitBreaker::new(cfg(true, 50.0, 5));
        assert!(cb.check(-49.9).is_ok());
        assert!(cb.check(-50.1).is_err());
        assert!(cb.is_halted());
        // halt 不可逆：即使 pnl 恢复也无法再下单
        assert!(cb.check(0.0).is_err());
    }

    #[test]
    fn disabled_is_noop() {
        let cb = CircuitBreaker::new(cfg(false, 0.0001, 1));
        cb.record_error();
        cb.record_error();
        assert!(!cb.is_halted());
        assert!(cb.check(-9999.0).is_ok());
    }
}
