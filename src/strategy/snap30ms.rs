/// 30ms 等间隔时间序列快照（v0.4.12-5m 新增）
/// 独立 tokio task 30ms tick 一次，从 AppState 读快照并按 5m 窗口落盘 CSV。
/// 完整性守门：第一个窗口（启动残缺）+ 最后一个窗口（退出残缺）丢弃，只保留完整 5m 窗口数据。
use std::fs;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::time::{interval, MissedTickBehavior};

use crate::tui::app::AppState;

const MAX_WINDOWS: usize = 10;
const SNAPSHOT_DIR: &str = "snapshots_30ms";
const TICK_MS: u64 = 30;

#[derive(Clone, Debug)]
pub struct Snap30Row {
    pub t_ms: i64,
    pub binance_mid: f64,
    pub fv_up: f64,
    pub fv_down: f64,
    pub poly_up_bid: f64,
    pub poly_up_ask: f64,
    pub poly_down_bid: f64,
    pub poly_down_ask: f64,
    pub sigma: f64,
    pub strike: f64,
    pub expiry_min: f64,
    /// 'S' = steady, 'E' = excited
    pub market_state: u8,
    pub window_end_ts: i64,
}

pub struct Snap30msWriter {
    buffer: Vec<Snap30Row>,
    current_window_end_ts: i64,
    output_dir: PathBuf,
    max_windows: usize,
    /// 标记当前 buffer 是否是启动后第一个窗口（残缺，不写盘）
    is_first_window: bool,
}

impl Snap30msWriter {
    pub fn new() -> Self {
        let output_dir = PathBuf::from(SNAPSHOT_DIR);
        let _ = fs::create_dir_all(&output_dir);
        Self {
            buffer: Vec::with_capacity(11_000), // 30ms × 5min ≈ 10000 rows
            current_window_end_ts: 0,
            output_dir,
            max_windows: MAX_WINDOWS,
            is_first_window: true,
        }
    }

    pub fn push(&mut self, row: Snap30Row) {
        let win = row.window_end_ts;
        if win <= 0 {
            return;
        }
        // 窗口切换：判断旧窗口是否完整
        if self.current_window_end_ts != 0 && win != self.current_window_end_ts {
            if !self.is_first_window {
                // 旧窗口完整 → flush
                self.flush();
                self.rotate();
            } else {
                // 启动后的第一个窗口（残缺）→ 丢弃
                tracing::info!("snap30ms: discarded incomplete window {}", self.current_window_end_ts);
            }
            self.buffer.clear();
            self.is_first_window = false;
        }
        self.current_window_end_ts = win;
        self.buffer.push(row);
    }

    fn flush(&self) {
        if self.current_window_end_ts <= 0 || self.buffer.is_empty() {
            return;
        }
        let path = self
            .output_dir
            .join(format!("snap_{}.csv", self.current_window_end_ts));
        let Ok(f) = fs::File::create(&path) else {
            return;
        };
        let mut w = BufWriter::new(f);
        let _ = writeln!(
            w,
            "t_ms,binance_mid,fv_up,fv_down,poly_up_bid,poly_up_ask,poly_down_bid,poly_down_ask,sigma,strike,expiry_min,market_state,window_end_ts"
        );
        for r in &self.buffer {
            let st = if r.market_state == b'E' { 'E' } else { 'S' };
            let _ = writeln!(
                w,
                "{},{:.4},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.4},{:.4},{},{}",
                r.t_ms,
                r.binance_mid,
                r.fv_up,
                r.fv_down,
                r.poly_up_bid,
                r.poly_up_ask,
                r.poly_down_bid,
                r.poly_down_ask,
                r.sigma,
                r.strike,
                r.expiry_min,
                st,
                r.window_end_ts,
            );
        }
        let _ = w.flush();
        tracing::info!(
            "snap30ms: flushed {} rows to {}",
            self.buffer.len(),
            path.display()
        );
    }

    fn rotate(&self) {
        let Ok(entries) = fs::read_dir(&self.output_dir) else {
            return;
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.starts_with("snap_") && n.ends_with(".csv") {
                    Some(n)
                } else {
                    None
                }
            })
            .collect();
        names.sort();
        while names.len() > self.max_windows {
            if let Some(old) = names.first() {
                let p = self.output_dir.join(old);
                let _ = fs::remove_file(p);
            }
            names.remove(0);
        }
    }
}

impl Default for Snap30msWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// 启动 30ms 快照 task
/// 注意：当前 in-flight 窗口的 buffer 在程序退出时不 flush（残缺，丢弃）—— 符合"只保留完整窗口"语义。
pub async fn run_snapshot_task(state: Arc<RwLock<AppState>>) {
    let mut writer = Snap30msWriter::new();
    let mut tick = interval(Duration::from_millis(TICK_MS));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    tracing::info!("snap30ms: started, interval={}ms, dir={}", TICK_MS, SNAPSHOT_DIR);

    loop {
        tick.tick().await;
        let row = if let Ok(s) = state.read() {
            // 跳过未连接前的脏数据
            if s.poly_window_end_ts <= 0 || s.mid_price <= 0.0 {
                continue;
            }
            Snap30Row {
                t_ms: chrono::Utc::now().timestamp_millis(),
                binance_mid: s.mid_price,
                fv_up: s.fair_price,
                fv_down: s.fair_price_down,
                poly_up_bid: s.poly_best_bid,
                poly_up_ask: s.poly_best_ask,
                poly_down_bid: s.poly_down_best_bid,
                poly_down_ask: s.poly_down_best_ask,
                sigma: s.volatility_annual,
                strike: s.strike_price,
                expiry_min: s.expiry_minutes,
                market_state: if s.market_state == "稳态" { b'S' } else { b'E' },
                window_end_ts: s.poly_window_end_ts,
            }
        } else {
            continue;
        };
        writer.push(row);
    }
}
