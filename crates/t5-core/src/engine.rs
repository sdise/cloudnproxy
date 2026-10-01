//! 引擎：监听本地 SOCKS5，逐连接建立隧道并双向转发。
//!
//! 相比最初版本，这里补齐了三项运行期能力：
//!
//! - **断线重连**（`auto_reconnect`）：单条连接的上游建立失败时按 300ms/600ms
//!   退避重试，最多 3 次；
//! - **自动切换**（`auto_switch`）：上游连续失败达阈值后，自动改选节点库中
//!   评分最高（速度优先、其次延迟）的可用节点，并清空隧道池；
//! - **隧道池**（`tunnel_pool`）：对同一目标预建一条备用隧道，下一次同目标
//!   连接可直接复用，省掉一次握手往返。
//!
//! 转发采用两条独立的「泵」任务放在同一个 `select!` 中：任一方结束即整体收尾，
//! 字节数在每次读写后立即累加到原子计数，因此长连接也能反映实时速率。

use crate::config::{Config, Node};
use crate::logbuf::LogSink;
use crate::outbound;
use crate::socks5;
use crate::stats::Stats;
use crate::tunnel_pool::TunnelPool;
use serde::Serialize;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

/// 转发缓冲区大小：32 KiB 在吞吐与内存占用间取平衡。
const BUF_SIZE: usize = 32 * 1024;
/// 连续失败多少次后触发自动切换。
const FAIL_THRESHOLD: u32 = 3;
/// 单条连接最多尝试几次上游。
const RETRY_ATTEMPTS: u32 = 3;
/// 每个目标缓存的备用隧道数。
const POOL_CAP_PER_KEY: usize = 4;
/// 备用隧道空闲多久后作废。
const POOL_IDLE: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, Serialize)]
pub struct EngineStatus {
    pub running: bool,
    pub addr: String,
    pub upstream: String,
    pub chain: String,
    pub tunnel_pool: usize,
}

/// 引擎共享上下文。
struct Ctx {
    cfg: Arc<Config>,
    /// 当前上游节点，可运行期切换
    upstream: RwLock<String>,
    /// 连续失败计数
    fails: AtomicU32,
    stats: Stats,
    logs: LogSink,
    pool: Option<Arc<TunnelPool>>,
}

impl Ctx {
    fn upstream(&self) -> String {
        self.upstream
            .read()
            .map(|s| s.clone())
            .unwrap_or_else(|_| self.cfg.upstream_addr())
    }

    fn set_upstream(&self, addr: &str) {
        if let Ok(mut g) = self.upstream.write() {
            *g = addr.to_string();
        }
        if let Some(pool) = &self.pool {
            pool.clear();
        }
    }

    fn note_success(&self) {
        self.fails.store(0, Ordering::Relaxed);
    }

    fn note_failure(&self) {
        let n = self.fails.fetch_add(1, Ordering::Relaxed) + 1;
        if !self.cfg.auto_switch || n < FAIL_THRESHOLD {
            return;
        }
        let current = self.upstream();
        match pick_best(&self.cfg.nodes, &current) {
            Some(best) => {
                self.set_upstream(&best);
                self.fails.store(0, Ordering::Relaxed);
                self.logs
                    .warn(format!("上游连续失败 {n} 次，已自动切换到 {best}"));
            }
            None => {
                self.logs
                    .warn(format!("上游连续失败 {n} 次，但节点库中没有可用的替代节点"));
                self.fails.store(0, Ordering::Relaxed);
            }
        }
    }
}

/// 选取评分最高的可用节点：速度优先，其次是延迟。
fn pick_best(nodes: &[Node], current: &str) -> Option<String> {
    let mut best: Option<(&Node, f64)> = None;
    for n in nodes {
        let addr = n.addr();
        if addr == current {
            continue;
        }
        if n.latency_ms.is_none() && n.speed_mbps.is_none() {
            continue;
        }
        let score = n.speed_mbps.unwrap_or(0.0) * 1000.0
            - n.latency_ms.unwrap_or(9_999) as f64;
        if best.map(|(_, s)| score > s).unwrap_or(true) {
            best = Some((n, score));
        }
    }
    best.map(|(n, _)| n.addr())
}

pub struct EngineHandle {
    stop: Arc<Notify>,
    running: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
    addr: String,
    chain: String,
    ctx: Arc<Ctx>,
}

impl EngineHandle {
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn upstream(&self) -> String {
        self.ctx.upstream()
    }

    /// 运行期切换上游节点，不中断已有连接。
    pub fn set_upstream(&self, addr: &str) {
        self.ctx.set_upstream(addr);
    }

    pub fn status(&self) -> EngineStatus {
        EngineStatus {
            running: self.is_running(),
            addr: self.addr.clone(),
            upstream: self.ctx.upstream(),
            chain: self.chain.clone(),
            tunnel_pool: self.ctx.pool.as_ref().map(|p| p.len()).unwrap_or(0),
        }
    }

    /// 请求停止并等待监听循环退出。
    pub async fn shutdown(self) {
        self.stop.notify_waiters();
        let _ = self.task.await;
    }
}

/// 启动引擎。返回的句柄负责停止、状态查询与节点热切换。
pub async fn start(cfg: Config, logs: LogSink, stats: Stats) -> io::Result<EngineHandle> {
    let listen = cfg.listen_addr();
    let listener = TcpListener::bind(&listen).await?;
    let addr = listener.local_addr().map(|a| a.to_string()).unwrap_or(listen);

    let cfg = Arc::new(cfg);
    let chain = if cfg.chain_enabled && !cfg.chain_addr.trim().is_empty() {
        cfg.chain_addr.trim().to_string()
    } else {
        "直连".to_string()
    };

    let pool = if cfg.tunnel_pool {
        Some(Arc::new(TunnelPool::new(POOL_CAP_PER_KEY, POOL_IDLE)))
    } else {
        None
    };

    let ctx = Arc::new(Ctx {
        upstream: RwLock::new(cfg.upstream_addr()),
        fails: AtomicU32::new(0),
        cfg: Arc::clone(&cfg),
        stats: stats.clone(),
        logs: logs.clone(),
        pool,
    });

    let stop = Arc::new(Notify::new());
    let running = Arc::new(AtomicBool::new(true));

    let banner = format!(
        "engine 已启动：{addr} → {}（{chain}）{}",
        ctx.upstream(),
        if ctx.pool.is_some() { " · 隧道池已启用" } else { "" }
    );

    let task = {
        let stop = Arc::clone(&stop);
        let running = Arc::clone(&running);
        let logs = logs.clone();
        let stats = stats.clone();
        let ctx = Arc::clone(&ctx);
        tokio::spawn(async move {
            logs.info(banner);
            loop {
                tokio::select! {
                    _ = stop.notified() => break,
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((sock, _peer)) => {
                                let max = ctx.cfg.max_conns;
                                if max > 0 && stats.conns() >= max as i64 {
                                    logs.warn(format!("并发已达上限 {max}，拒绝新连接"));
                                    continue;
                                }
                                let ctx = Arc::clone(&ctx);
                                tokio::spawn(async move { handle(sock, ctx).await });
                            }
                            Err(e) => logs.warn(format!("accept 失败: {e}")),
                        }
                    }
                }
            }
            stats.reset_conns();
            running.store(false, Ordering::SeqCst);
            logs.info("engine 已停止");
        })
    };

    Ok(EngineHandle {
        stop,
        running,
        task,
        addr,
        chain,
        ctx,
    })
}

async fn handle(mut client: TcpStream, ctx: Arc<Ctx>) {
    if ctx.cfg.tcp_nodelay {
        let _ = client.set_nodelay(true);
    }

    let target = match socks5::handshake(&mut client).await {
        Ok(t) => t,
        Err(e) => {
            ctx.logs.debug(format!("SOCKS5 握手失败: {e}"));
            return;
        }
    };
    let authority = target.authority();

    let started = Instant::now();
    let up = match connect_with_retry(&ctx, &authority).await {
        Ok(s) => s,
        Err(e) => {
            ctx.logs.warn(format!("CONNECT {authority} → {e}"));
            ctx.note_failure();
            let _ = socks5::reply(&mut client, socks5::REP_CONN_REFUSED).await;
            return;
        }
    };

    ctx.note_success();
    let rtt = started.elapsed().as_millis();
    if socks5::reply(&mut client, socks5::REP_OK).await.is_err() {
        return;
    }
    ctx.logs.info(format!("CONNECT {authority} → OK ({rtt}ms)"));
    ctx.stats.inc_conns();

    let (client_r, client_w) = tokio::io::split(client);
    let (up_r, up_w) = tokio::io::split(up);

    let s_up = ctx.stats.clone();
    let s_down = ctx.stats.clone();
    tokio::select! {
        _ = pump(client_r, up_w, s_up, true) => {},
        _ = pump(up_r, client_w, s_down, false) => {},
    }

    ctx.stats.dec_conns();
}

/// 建立上游隧道：优先复用池，失败则退避重试。
async fn connect_with_retry(ctx: &Ctx, authority: &str) -> io::Result<TcpStream> {
    let attempts = if ctx.cfg.auto_reconnect {
        RETRY_ATTEMPTS
    } else {
        1
    };
    let mut last: Option<io::Error> = None;

    for attempt in 0..attempts {
        let upstream = ctx.upstream();

        if let Some(pool) = &ctx.pool {
            let key = TunnelPool::key(&upstream, authority);
            if let Some(s) = pool.take(&key) {
                ctx.logs.debug(format!("复用已建隧道 {authority}"));
                return Ok(s);
            }
        }

        match outbound::connect_tunnel(&ctx.cfg, &upstream, authority).await {
            Ok(s) => {
                if let Some(pool) = &ctx.pool {
                    let key = TunnelPool::key(&upstream, authority);
                    pool.prime(Arc::clone(&ctx.cfg), key, authority.to_string());
                }
                return Ok(s);
            }
            Err(e) => {
                if attempt + 1 < attempts {
                    ctx.logs
                        .debug(format!("{authority} 连接失败（{e}），准备重试"));
                    tokio::time::sleep(Duration::from_millis(300 * (attempt as u64 + 1)))
                        .await;
                }
                last = Some(e);
            }
        }
    }

    Err(last.unwrap_or_else(|| io::Error::new(io::ErrorKind::Other, "上游连接失败")))
}

/// 单向转发。`is_up` 为真时计入上行字节。
async fn pump<R, W>(mut r: R, mut w: W, stats: Stats, is_up: bool)
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut buf = vec![0u8; BUF_SIZE];
    loop {
        let n = match r.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        if w.write_all(&buf[..n]).await.is_err() {
            break;
        }
        if is_up {
            stats.add_up(n as u64);
        } else {
            stats.add_down(n as u64);
        }
    }
    let _ = w.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(ip: &str, speed: Option<f64>, lat: Option<u32>) -> Node {
        Node {
            ip: ip.to_string(),
            port: 443,
            speed_mbps: speed,
            latency_ms: lat,
            ..Default::default()
        }
    }

    #[test]
    fn picks_fastest_node_skipping_current() {
        let nodes = vec![
            node("1.1.1.1", Some(10.0), Some(10)),
            node("2.2.2.2", Some(99.0), Some(50)),
            node("3.3.3.3", None, None),
        ];
        assert_eq!(pick_best(&nodes, "2.2.2.2:443"), Some("1.1.1.1:443".to_string()));
    }

    #[test]
    fn prefers_low_latency_when_speeds_equal() {
        let nodes = vec![
            node("1.1.1.1", Some(50.0), Some(80)),
            node("2.2.2.2", Some(50.0), Some(20)),
        ];
        assert_eq!(pick_best(&nodes, ""), Some("2.2.2.2:443".to_string()));
    }
}
