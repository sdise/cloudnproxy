//! 内存环形日志缓冲。
//!
//! 日志**不写入任何文件**：只保存在固定容量的 `VecDeque` 中，供 UI 查询，
//! 并通过可选的 emitter 回调实时推送到前端。

use serde::Serialize;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize)]
pub struct LogLine {
    /// Unix 毫秒时间戳
    pub ts: u64,
    pub level: String,
    pub msg: String,
}

type Emitter = Arc<dyn Fn(LogLine) + Send + Sync>;

#[derive(Clone)]
pub struct LogSink {
    inner: Arc<Mutex<VecDeque<LogLine>>>,
    cap: usize,
    emitter: Arc<Mutex<Option<Emitter>>>,
}

impl Default for LogSink {
    fn default() -> Self {
        Self::new(2000)
    }
}

impl LogSink {
    pub fn new(cap: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(cap.min(256)))),
            cap: cap.max(16),
            emitter: Arc::new(Mutex::new(None)),
        }
    }

    /// 注册实时推送回调（Tauri 侧用来 emit 事件）。
    pub fn set_emitter<F>(&self, f: F)
    where
        F: Fn(LogLine) + Send + Sync + 'static,
    {
        if let Ok(mut slot) = self.emitter.lock() {
            *slot = Some(Arc::new(f));
        }
    }

    pub fn log(&self, level: &str, msg: impl Into<String>) {
        let line = LogLine {
            ts: now_ms(),
            level: level.to_string(),
            msg: msg.into(),
        };
        if let Ok(mut buf) = self.inner.lock() {
            if buf.len() >= self.cap {
                buf.pop_front();
            }
            buf.push_back(line.clone());
        }
        if let Ok(slot) = self.emitter.lock() {
            if let Some(f) = slot.as_ref() {
                f(line);
            }
        }
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

    pub fn debug(&self, msg: impl Into<String>) {
        self.log("DEBUG", msg);
    }

    pub fn snapshot(&self) -> Vec<LogLine> {
        self.inner
            .lock()
            .map(|b| b.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn clear(&self) {
        if let Ok(mut buf) = self.inner.lock() {
            buf.clear();
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
