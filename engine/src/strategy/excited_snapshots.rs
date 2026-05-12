/// 激变态快照：按事件记录 FV 与 Poly 盘口，按 5 分钟窗口写 CSV，保留最近 N 个窗口
use std::fs;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

const MAX_WINDOWS: usize = 10;
const SNAPSHOT_DIR: &str = "excited_snapshots";

/// 单条快照（激变态下某时刻的 FV 与 Poly 价格）
#[derive(Clone, Debug)]
pub struct SnapshotRow {
    pub t_ms: i64,
    pub fv_up: f64,
    pub fv_down: f64,
    pub poly_up_bid: f64,
    pub poly_up_ask: f64,
    pub poly_down_bid: f64,
    pub poly_down_ask: f64,
    /// 事件来源：B=Binance(BookTicker), P=Poly(PolyBookUpdate)
    pub source: u8,
}

/// 按窗口缓冲快照，窗口切换时落盘并只保留最近 MAX_WINDOWS 个文件
pub struct ExcitedSnapshotWriter {
    buffer: Vec<SnapshotRow>,
    current_window_end_ts: i64,
    output_dir: PathBuf,
    max_windows: usize,
}

impl ExcitedSnapshotWriter {
    pub fn new() -> Self {
        let output_dir = PathBuf::from(SNAPSHOT_DIR);
        let _ = fs::create_dir_all(&output_dir);
        Self {
            buffer: Vec::with_capacity(2000),
            current_window_end_ts: 0,
            output_dir,
            max_windows: MAX_WINDOWS,
        }
    }

    /// 写入一条快照；若窗口变化则先落盘上一窗口并做轮转
    pub fn push_snapshot(&mut self, window_end_ts: i64, row: SnapshotRow) {
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

    /// 将当前缓冲写入 excited_<window_end_ts>.csv
    fn flush(&mut self) {
        if self.current_window_end_ts <= 0 || self.buffer.is_empty() {
            return;
        }
        let path = self
            .output_dir
            .join(format!("excited_{}.csv", self.current_window_end_ts));
        let Ok(f) = fs::File::create(&path) else {
            return;
        };
        let mut w = BufWriter::new(f);
        let _ = writeln!(
            w,
            "t_ms,fv_up,fv_down,poly_up_bid,poly_up_ask,poly_down_bid,poly_down_ask,source"
        );
        for r in &self.buffer {
            let src = if r.source == b'P' { "P" } else { "B" };
            let _ = writeln!(
                w,
                "{},{:.6},{:.6},{:.6},{:.6},{:.6},{:.6},{}",
                r.t_ms,
                r.fv_up,
                r.fv_down,
                r.poly_up_bid,
                r.poly_up_ask,
                r.poly_down_bid,
                r.poly_down_ask,
                src
            );
        }
        let _ = w.flush();
    }

    /// 只保留最近 max_windows 个 excited_*.csv，删除更早的
    fn rotate(&mut self) {
        let Ok(entries) = fs::read_dir(&self.output_dir) else {
            return;
        };
        let mut names: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.starts_with("excited_") && n.ends_with(".csv") {
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

impl Default for ExcitedSnapshotWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ExcitedSnapshotWriter {
    fn drop(&mut self) {
        self.flush();
    }
}
