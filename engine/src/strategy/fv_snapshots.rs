/// 全量 FV/Poly 快照采集器（v0.3.3-5m 新增）
/// 每个 BookTicker / PolyBookUpdate 事件都落盘，毫秒级时间戳，按 5 分钟窗口轮转。
/// 用途：后置 lead-lag 分析（FV 是否领先 Poly_p、领先多少 ms、偏差分布）。
use std::fs;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

const MAX_WINDOWS: usize = 6;
const SNAPSHOT_DIR: &str = "fv_snapshots";

#[derive(Clone, Debug)]
pub struct FvRow {
    pub t_ms: i64,
    /// 'B' = Binance BookTicker, 'P' = Poly book update
    pub source: u8,
    pub binance_mid: f64,
    pub fv_up: f64,
    pub fv_down: f64,
    pub poly_up_bid: f64,
    pub poly_up_ask: f64,
    pub poly_down_bid: f64,
    pub poly_down_ask: f64,
    pub sigma: f64,
    pub iv_raw: f64,
    pub sigma_ema: f64,
    /// 'S' = steady, 'E' = excited
    pub market_state: u8,
    pub expiry_min: f64,
    pub strike: f64,
}

pub struct FvSnapshotWriter {
    buffer: Vec<FvRow>,
    current_window_end_ts: i64,
    output_dir: PathBuf,
    max_windows: usize,
}

impl FvSnapshotWriter {
    pub fn new() -> Self {
        let output_dir = PathBuf::from(SNAPSHOT_DIR);
        let _ = fs::create_dir_all(&output_dir);
        Self {
            buffer: Vec::with_capacity(8000),
            current_window_end_ts: 0,
            output_dir,
            max_windows: MAX_WINDOWS,
        }
    }

    pub fn push(&mut self, window_end_ts: i64, row: FvRow) {
        if window_end_ts <= 0 {
            return;
        }
        if self.current_window_end_ts != 0 && window_end_ts != self.current_window_end_ts {
            self.flush();
            self.rotate();
            self.buffer.clear();
        }
        self.current_window_end_ts = window_end_ts;
        self.buffer.push(row);
    }

    fn flush(&mut self) {
        if self.current_window_end_ts <= 0 || self.buffer.is_empty() {
            return;
        }
        let path = self
            .output_dir
            .join(format!("fv_{}.csv", self.current_window_end_ts));
        let Ok(f) = fs::File::create(&path) else {
            return;
        };
        let mut w = BufWriter::new(f);
        let _ = writeln!(
            w,
            "t_ms,source,binance_mid,fv_up,fv_down,poly_up_bid,poly_up_ask,poly_down_bid,poly_down_ask,sigma,iv_raw,sigma_ema,market_state,expiry_min,strike"
        );
        for r in &self.buffer {
            let src = if r.source == b'P' { 'P' } else { 'B' };
            let st = if r.market_state == b'E' { 'E' } else { 'S' };
            let _ = writeln!(
                w,
                "{},{},{:.4},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{},{:.4},{:.4}",
                r.t_ms,
                src,
                r.binance_mid,
                r.fv_up,
                r.fv_down,
                r.poly_up_bid,
                r.poly_up_ask,
                r.poly_down_bid,
                r.poly_down_ask,
                r.sigma,
                r.iv_raw,
                r.sigma_ema,
                st,
                r.expiry_min,
                r.strike,
            );
        }
        let _ = w.flush();
    }

    fn rotate(&mut self) {
        let Ok(entries) = fs::read_dir(&self.output_dir) else {
            return;
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.starts_with("fv_") && n.ends_with(".csv") {
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

impl Default for FvSnapshotWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for FvSnapshotWriter {
    fn drop(&mut self) {
        self.flush();
    }
}
