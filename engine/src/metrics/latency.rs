/// metrics/latency.rs - 延迟统计监控
/// 使用 HdrHistogram 记录 P50/P99/Max 延迟分布
/// 每隔 report_interval_secs 打印一次统计报告
use hdrhistogram::Histogram;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tracing::{info, warn};

use crate::tui::app::LatencySnapshot;

/// 延迟记录器（跨线程共享）
#[derive(Clone)]
pub struct LatencyMonitor {
    inner: Arc<Mutex<LatencyInner>>,
    warn_threshold_ms: u64,
    report_interval: Duration,
}

struct LatencyInner {
    /// 网络延迟（交易所时间戳 → 本机收到）单位 μs
    network_hist: Histogram<u64>,
    /// 消息解析延迟（单位 μs）
    parse_hist: Histogram<u64>,
    /// 上次打印时间
    last_report: Instant,
    /// 本周期消息数
    msg_count: u64,
}

impl LatencyMonitor {
    /// 创建监控器
    /// - warn_threshold_ms: 超过此网络延迟打印 WARN
    /// - report_interval_secs: 统计报告间隔
    pub fn new(warn_threshold_ms: u64, report_interval_secs: u64) -> Self {
        let inner = LatencyInner {
            // 最大记录 60秒 = 60_000_000 μs，精度 3 位有效数字
            network_hist: Histogram::new_with_bounds(1, 60_000_000, 3)
                .expect("network_hist init failed"),
            parse_hist: Histogram::new_with_bounds(1, 1_000_000, 3)
                .expect("parse_hist init failed"),
            last_report: Instant::now(),
            msg_count: 0,
        };
        Self {
            inner: Arc::new(Mutex::new(inner)),
            warn_threshold_ms,
            report_interval: Duration::from_secs(report_interval_secs),
        }
    }

    /// 记录一条消息的延迟
    /// - exchange_ts_ms: 交易所事件时间戳（毫秒）
    /// - recv_ts_ns: 本机收到时间（纳秒，来自 std::time::SystemTime）
    /// - parse_us: 消息解析耗时（微秒）
    pub fn record(&self, exchange_ts_ms: u64, recv_ts_ns: u128, parse_us: u64) {
        // 计算网络延迟
        let recv_ms = (recv_ts_ns / 1_000_000) as u64;
        let network_us = recv_ms.saturating_sub(exchange_ts_ms).saturating_mul(1000);

        // 超阈值告警
        if network_us / 1000 > self.warn_threshold_ms {
            warn!(
                "[LATENCY⚠️] network={}ms parse={}μs  (threshold={}ms)",
                network_us / 1000,
                parse_us,
                self.warn_threshold_ms
            );
        }

        let mut inner = self.inner.lock().unwrap();
        let _ = inner.network_hist.record(network_us.max(1));
        let _ = inner.parse_hist.record(parse_us.max(1));
        inner.msg_count += 1;

        // 检查是否需要打印报告
        if inner.last_report.elapsed() >= self.report_interval {
            self.print_report(&inner);
            // 重置
            inner.network_hist.reset();
            inner.parse_hist.reset();
            inner.last_report = Instant::now();
            inner.msg_count = 0;
        }
    }

    /// 记录一条消息的延迟，并返回当前快照（每条消息山返回，供 TUI 展示）
    pub fn record_and_snapshot(
        &self,
        exchange_ts_ms: u64,
        recv_ts_ns: u128,
        parse_us: u64,
    ) -> Option<LatencySnapshot> {
        let recv_ms = (recv_ts_ns / 1_000_000) as u64;
        let network_us = recv_ms.saturating_sub(exchange_ts_ms).saturating_mul(1000);

        if network_us / 1000 > self.warn_threshold_ms {
            warn!(
                "[LATENCY⚠️] network={}ms parse={}μs  (threshold={}ms)",
                network_us / 1000,
                parse_us,
                self.warn_threshold_ms
            );
        }

        let mut inner = self.inner.lock().unwrap();
        let _ = inner.network_hist.record(network_us.max(1));
        let _ = inner.parse_hist.record(parse_us.max(1));
        inner.msg_count += 1;

        // 返回当前快照
        let snap = LatencySnapshot {
            p50_ms: inner.network_hist.value_at_quantile(0.50) as f64 / 1000.0,
            p99_ms: inner.network_hist.value_at_quantile(0.99) as f64 / 1000.0,
            max_ms: inner.network_hist.max() as f64 / 1000.0,
            parse_p99_us: inner.parse_hist.value_at_quantile(0.99) as f64,
            msg_count: inner.msg_count,
        };

        // 定期重置直方图
        if inner.last_report.elapsed() >= self.report_interval {
            inner.network_hist.reset();
            inner.parse_hist.reset();
            inner.last_report = Instant::now();
            inner.msg_count = 0;
        }

        Some(snap)
    }

    fn print_report(&self, inner: &LatencyInner) {
        let net = &inner.network_hist;
        let parse = &inner.parse_hist;

        info!(
            "══════════════════════════════════════════\n\
             📊 延迟统计报告 ({}条消息)\n\
             ┌─[网络延迟] P50={:.2}ms  P99={:.2}ms  Max={:.2}ms\n\
             └─[解析延迟] P50={:.0}μs  P99={:.0}μs  Max={:.0}μs\n\
             ══════════════════════════════════════════",
            inner.msg_count,
            net.value_at_quantile(0.50) as f64 / 1000.0,
            net.value_at_quantile(0.99) as f64 / 1000.0,
            net.max() as f64 / 1000.0,
            parse.value_at_quantile(0.50) as f64,
            parse.value_at_quantile(0.99) as f64,
            parse.max() as f64,
        );
    }

    /// 手动触发打印（程序退出时调用）
    pub fn flush(&self) {
        let inner = self.inner.lock().unwrap();
        if inner.msg_count > 0 {
            self.print_report(&inner);
        }
    }
}
