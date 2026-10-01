//! 上游隧道池。
//!
//! 只缓存**已完成 CONNECT 但尚未传输任何数据**的隧道 —— 这一点很关键：
//! 一条 HTTP CONNECT 隧道一旦被写入请求数据，状态就不再干净，不能交给下一个
//! 使用者。因此这里的策略是「预建」：某目标的连接建立成功后，顺带为同一目标
//! 预建一条备用隧道放进池里，下次同目标连接直接取用，省掉一次握手往返。
//!
//! 池的键包含上游节点地址，节点被切换后旧池自然失效，不会被误用。

use crate::config::Config;
use crate::outbound;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

struct Entry {
    stream: TcpStream,
    created: Instant,
}

pub struct TunnelPool {
    inner: Mutex<HashMap<String, VecDeque<Entry>>>,
    /// 每个目标最多缓存的备用隧道数
    cap_per_key: usize,
    /// 空闲超过该时长即丢弃
    idle: Duration,
    inflight: AtomicUsize,
    /// 同时预建的隧道数上限，避免测速/扫描流量把节点打爆
    max_inflight: usize,
}

impl TunnelPool {
    pub fn new(cap_per_key: usize, idle: Duration) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            cap_per_key: cap_per_key.max(1),
            idle,
            inflight: AtomicUsize::new(0),
            max_inflight: 8,
        }
    }

    /// 组装池键：上游节点 + 目标。
    pub fn key(upstream: &str, authority: &str) -> String {
        format!("{upstream}|{authority}")
    }

    /// 取出可用的隧道；不可用（过期或对端已关闭）的会被丢弃。
    pub fn take(&self, key: &str) -> Option<TcpStream> {
        let mut map = self.inner.lock().ok()?;
        let queue = map.get_mut(key)?;
        while let Some(entry) = queue.pop_front() {
            if entry.created.elapsed() > self.idle {
                continue;
            }
            if is_alive(&entry.stream) {
                return Some(entry.stream);
            }
        }
        None
    }

    fn push(&self, key: String, stream: TcpStream) {
        if let Ok(mut map) = self.inner.lock() {
            let queue = map.entry(key).or_default();
            while queue.len() >= self.cap_per_key {
                queue.pop_front();
            }
            queue.push_back(Entry {
                stream,
                created: Instant::now(),
            });
        }
    }

    /// 异步预建一条备用隧道。立即返回，不阻塞当前连接。
    pub fn prime(self: &Arc<Self>, cfg: Arc<Config>, key: String, authority: String) {
        if self.inflight.load(Ordering::Relaxed) >= self.max_inflight {
            return;
        }
        let upstream = match key.split('|').next() {
            Some(u) if !u.is_empty() => u.to_string(),
            _ => return,
        };
        let this = Arc::clone(self);
        tokio::spawn(async move {
            this.inflight.fetch_add(1, Ordering::Relaxed);
            if let Ok(s) = outbound::connect_tunnel(&cfg, &upstream, &authority).await {
                this.push(key, s);
            }
            this.inflight.fetch_sub(1, Ordering::Relaxed);
        });
    }

    /// 清空全部缓存（节点切换时调用）。
    pub fn clear(&self) {
        if let Ok(mut map) = self.inner.lock() {
            map.clear();
        }
    }

    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .map(|m| m.values().map(|q| q.len()).sum())
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 判断隧道是否仍然可用：非阻塞读一次，只有「无数据可读」才算健康。
fn is_alive(stream: &TcpStream) -> bool {
    let mut buf = [0u8; 1];
    match stream.try_read(&mut buf) {
        // 对端已关闭
        Ok(0) => false,
        // 竟然有数据：状态不干净，丢弃
        Ok(_) => false,
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => true,
        Err(_) => false,
    }
}
