//! 转发统计。全部为原子计数，可在任意线程/任务中无锁更新。

use serde::Serialize;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Default)]
pub struct StatsInner {
    pub up: AtomicU64,
    pub down: AtomicU64,
    pub conns: AtomicI64,
    pub sessions: AtomicU64,
}

/// 句柄，克隆成本极低。
#[derive(Clone, Default)]
pub struct Stats(pub Arc<StatsInner>);

#[derive(Debug, Clone, Serialize, Default)]
pub struct StatsSnapshot {
    pub up_bytes: u64,
    pub down_bytes: u64,
    pub conns: i64,
    pub sessions: u64,
}

impl Stats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_up(&self, n: u64) {
        self.0.up.fetch_add(n, Ordering::Relaxed);
    }

    pub fn add_down(&self, n: u64) {
        self.0.down.fetch_add(n, Ordering::Relaxed);
    }

    pub fn inc_conns(&self) {
        self.0.conns.fetch_add(1, Ordering::Relaxed);
        self.0.sessions.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec_conns(&self) {
        self.0.conns.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn conns(&self) -> i64 {
        self.0.conns.load(Ordering::Relaxed)
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            up_bytes: self.0.up.load(Ordering::Relaxed),
            down_bytes: self.0.down.load(Ordering::Relaxed),
            conns: self.0.conns.load(Ordering::Relaxed),
            sessions: self.0.sessions.load(Ordering::Relaxed),
        }
    }

    /// 停止引擎时清零连接数（长连接被强制断开不会走 dec_conns）。
    pub fn reset_conns(&self) {
        self.0.conns.store(0, Ordering::Relaxed);
    }
}
