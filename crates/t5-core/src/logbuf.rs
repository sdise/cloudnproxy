//! 日志：内存环形缓冲 + 可选的文件落盘 + 可选的实时回调。
//!
//! 三种输出去向彼此独立：
//! - **环形缓冲**：始终启用，供 GUI 的「日志」页读取，容量固定，超出丢弃最旧记录；
//! - **文件**：仅当配置了 `log_file` 时才写入，由无 GUI 版本使用；
//! - **回调（emitter）**：GUI 推事件、CLI 打印到标准输出，都通过它实现。
//!
//! 级别过滤在入口处完成，低于当前级别的记录不会产生任何开销。

use serde::Serialize;
use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub const LEVEL_TRACE: u8 = 0;
pub const LEVEL_DEBUG: u8 = 1;
pub const LEVEL_INFO: u8 = 2;
pub const LEVEL_WARN: u8 = 3;
pub const LEVEL_ERROR: u8 = 4;

/// 把级别名转成数值（无法识别时按 INFO 处理）。
pub fn level_code(name: &str) -> u8 {
    match name.trim().to_ascii_uppercase().as_str() {
        "TRACE" => LEVEL_TRACE,
        "DEBUG" => LEVEL_DEBUG,
        "INFO" => LEVEL_INFO,
        "WARN" | "WARNING" => LEVEL_WARN,
        "ERROR" | "ERR" => LEVEL_ERROR,
        _ => LEVEL_INFO,
    }
}

pub fn level_name(code: u8) -> &'static str {
    match code {
        LEVEL_TRACE => "TRACE",
        LEVEL_DEBUG => "DEBUG",
        LEVEL_WARN => "WARN",
        LEVEL_ERROR => "ERROR",
        _ => "INFO",
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LogLine {
    /// Unix 毫秒时间戳
    pub ts: u64,
    pub level: String,
    pub msg: String,
}

type Emitter = Arc<dyn Fn(LogLine) + Send + Sync>;

struct Sink {
    ring: Mutex<VecDeque<LogLine>>,
    file: Mutex<Option<BufWriter<std::fs::File>>>,
    emitter: Mutex<Option<Emitter>>,
}

#[derive(Clone)]
pub struct LogSink {
    cap: usize,
    min: Arc<AtomicU8>,
    inner: Arc<Sink>,
}

impl Default for LogSink {
    fn default() -> Self {
        Self::new(2000)
    }
}

impl LogSink {
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(16),
            min: Arc::new(AtomicU8::new(LEVEL_INFO)),
            inner: Arc::new(Sink {
                ring: Mutex::new(VecDeque::with_capacity(cap.min(256))),
                file: Mutex::new(None),
                emitter: Mutex::new(None),
            }),
        }
    }

    /// 设置最低输出级别（低于该级别的记录直接丢弃）。
    pub fn set_min_level(&self, name: &str) {
        self.min.store(level_code(name), Ordering::Relaxed);
    }

    pub fn min_level(&self) -> u8 {
        self.min.load(Ordering::Relaxed)
    }

    /// 设置日志文件；传 `None` 关闭文件输出。
    pub fn set_file(&self, path: Option<PathBuf>) -> std::io::Result<()> {
        let mut slot = self
            .inner
            .file
            .lock()
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "日志文件锁已损坏"))?;
        *slot = None;
        if let Some(p) = path {
            if let Some(dir) = p.parent() {
                if !dir.as_os_str().is_empty() {
                    std::fs::create_dir_all(dir)?;
                }
            }
            let f = OpenOptions::new().create(true).append(true).open(&p)?;
            *slot = Some(BufWriter::new(f));
        }
        Ok(())
    }

    /// 注册实时回调：GUI 用来 emit 事件，CLI 用来写标准输出。
    pub fn set_emitter<F>(&self, f: F)
    where
        F: Fn(LogLine) + Send + Sync + 'static,
    {
        if let Ok(mut slot) = self.inner.emitter.lock() {
            *slot = Some(Arc::new(f));
        }
    }

    pub fn log(&self, level: &str, msg: impl Into<String>) {
        let code = level_code(level);
        if code < self.min.load(Ordering::Relaxed) {
            return;
        }
        let line = LogLine {
            ts: now_ms(),
            level: level_name(code).to_string(),
            msg: msg.into(),
        };

        if let Ok(mut ring) = self.inner.ring.lock() {
            if ring.len() >= self.cap {
                ring.pop_front();
            }
            ring.push_back(line.clone());
        }

        if let Ok(mut slot) = self.inner.file.lock() {
            if let Some(w) = slot.as_mut() {
                let _ = writeln!(
                    w,
                    "{} {:<5} {}",
                    iso_time(line.ts),
                    line.level,
                    line.msg
                );
                let _ = w.flush();
            }
        }

        if let Ok(slot) = self.inner.emitter.lock() {
            if let Some(f) = slot.as_ref() {
                f(line);
            }
        }
    }

    pub fn trace(&self, msg: impl Into<String>) {
        self.log("TRACE", msg);
    }

    pub fn debug(&self, msg: impl Into<String>) {
        self.log("DEBUG", msg);
    }

    pub fn info(&self, msg: impl Into<String>) {
        self.log("INFO", msg);
    }

    pub fn warn(&self, msg: impl Into<String>) {
        self.log("WARN", msg);
    }

    pub fn error(&self, msg: impl Into<String>) {
        self.log("ERROR", msg);
    }

    pub fn snapshot(&self) -> Vec<LogLine> {
        self.inner
            .ring
            .lock()
            .map(|b| b.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn clear(&self) {
        if let Ok(mut ring) = self.inner.ring.lock() {
            ring.clear();
        }
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `YYYY-MM-DD HH:MM:SS`（UTC）。
pub fn iso_time(ms: u64) -> String {
    let secs = ms / 1000;
    let tod = secs % 86_400;
    let days = (secs / 86_400) as i64;
    let (y, mo, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        y,
        mo,
        d,
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// 由「1970-01-01 起的天数」反推公历日期（Howard Hinnant 算法）。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_epoch() {
        assert_eq!(iso_time(0), "1970-01-01 00:00:00");
    }

    #[test]
    fn formats_known_timestamp() {
        // 2026-10-02 03:04:05 UTC
        let ms = 1_790_910_245_000u64;
        assert_eq!(iso_time(ms), "2026-10-02 03:04:05");
    }

    #[test]
    fn formats_leap_day() {
        // 2024-02-29 12:00:00 UTC
        assert_eq!(iso_time(1_709_208_000_000u64), "2024-02-29 12:00:00");
    }

    #[test]
    fn level_filtering_works() {
        let sink = LogSink::new(16);
        sink.set_min_level("warn");
        sink.info("看不到");
        sink.warn("看得到");
        let lines = sink.snapshot();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].msg, "看得到");
    }
}
