//! 安全熔断：cash_pnl 亏损或连续下单错误超阈值时永久阻止 LiveOrderClient 继续下单。
//!
//! 仿 poly-kalshi-arb-main/src/circuit_breaker.rs 的代码风格，仅保留 MaxDailyLoss
//! 与 ConsecutiveErrors 两类触发；无 cooldown / reset，需重启进程恢复。
//! 内部状态全部基于 Atomic + std::sync::RwLock，可在同步热路径 (dispatch_buy_intent)
//! 直接调用，不被 async 染色。

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::RwLock;

use tracing::{error, info};

#[derive(Debug, Clone)]
pub struct CircuitBreakerConfig {
    pub max_daily_loss: f64,
    pub max_consecutive_errors: u32,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TripReason {
    MaxDailyLoss { loss: f64, limit: f64 },
    ConsecutiveErrors { count: u32, limit: u32 },
}

impl std::fmt::Display for TripReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TripReason::MaxDailyLoss { loss, limit } => {
                write!(f, "Max daily loss: ${:.2} (limit: ${:.2})", loss, limit)
            }
            TripReason::ConsecutiveErrors { count, limit } => {
                write!(f, "Consecutive errors: {} (limit: {})", count, limit)
            }
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
        info!("[CB] Circuit breaker initialized:");
        info!("[CB]   Enabled: {}", config.enabled);
        info!("[CB]   Max daily loss: ${:.2}", config.max_daily_loss);
        info!("[CB]   Max consecutive errors: {}", config.max_consecutive_errors);
        Self {
            config,
            halted: AtomicBool::new(false),
            trip_reason: RwLock::new(None),
            consecutive_errors: AtomicI64::new(0),
        }
    }

    #[inline]
    pub fn is_halted(&self) -> bool {
        self.config.enabled && self.halted.load(Ordering::SeqCst)
    }

    pub fn trip_reason(&self) -> Option<TripReason> {
        self.trip_reason.read().ok().and_then(|g| g.clone())
    }

    /// 同步检查：halted 或 cash_pnl 亏损越线则返回 Err（必要时同步触发 trip）。
    pub fn check(&self, cash_pnl: f64) -> Result<(), TripReason> {
        if !self.config.enabled {
            return Ok(());
        }
        if self.halted.load(Ordering::SeqCst) {
            return Err(self.trip_reason().unwrap_or(TripReason::ConsecutiveErrors {
                count: 0,
                limit: self.config.max_consecutive_errors,
            }));
        }
        let loss = -cash_pnl;
        if loss > self.config.max_daily_loss {
            let reason = TripReason::MaxDailyLoss {
                loss,
                limit: self.config.max_daily_loss,
            };
            self.trip(reason.clone());
            return Err(reason);
        }
        Ok(())
    }

    pub fn record_success(&self) {
        self.consecutive_errors.store(0, Ordering::SeqCst);
    }

    pub fn record_error(&self) {
        let n = self.consecutive_errors.fetch_add(1, Ordering::SeqCst) + 1;
        if n >= self.config.max_consecutive_errors as i64 {
            self.trip(TripReason::ConsecutiveErrors {
                count: n as u32,
                limit: self.config.max_consecutive_errors,
            });
        }
    }

    fn trip(&self, reason: TripReason) {
        if !self.config.enabled {
            return;
        }
        if self.halted.swap(true, Ordering::SeqCst) {
            return;
        }
        error!("🚨 CIRCUIT BREAKER TRIPPED: {}", reason);
        if let Ok(mut g) = self.trip_reason.write() {
            *g = Some(reason);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(loss: f64, errs: u32) -> CircuitBreakerConfig {
        CircuitBreakerConfig {
            max_daily_loss: loss,
            max_consecutive_errors: errs,
            enabled: true,
        }
    }

    #[test]
    fn consecutive_errors_trip_at_threshold() {
        let cb = CircuitBreaker::new(cfg(100.0, 5));
        for _ in 0..4 {
            cb.record_error();
            assert!(!cb.is_halted());
        }
        cb.record_error();
        assert!(cb.is_halted());
        assert!(matches!(
            cb.check(0.0),
            Err(TripReason::ConsecutiveErrors { count: 5, limit: 5 })
        ));
    }

    #[test]
    fn daily_loss_trips_and_blocks_subsequent_checks() {
        let cb = CircuitBreaker::new(cfg(50.0, 5));
        assert!(cb.check(-49.0).is_ok());
        assert!(matches!(
            cb.check(-51.0),
            Err(TripReason::MaxDailyLoss { .. })
        ));
        assert!(cb.is_halted());
        // 即便 pnl 恢复，halted 状态依旧拒绝
        assert!(cb.check(0.0).is_err());
    }

    #[test]
    fn record_success_resets_consecutive_errors() {
        let cb = CircuitBreaker::new(cfg(100.0, 3));
        cb.record_error();
        cb.record_error();
        cb.record_success();
        cb.record_error();
        cb.record_error();
        assert!(!cb.is_halted());
    }

    #[test]
    fn disabled_breaker_never_trips() {
        let mut c = cfg(1.0, 1);
        c.enabled = false;
        let cb = CircuitBreaker::new(c);
        cb.record_error();
        assert!(cb.check(-100.0).is_ok());
        assert!(!cb.is_halted());
    }
}
