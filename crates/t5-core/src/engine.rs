//! 引擎：监听本地 SOCKS5，逐连接建立隧道并双向转发。
//!
//! 转发采用两条独立的「泵」任务放在同一个 `select!` 中：任一方结束即整体收尾，
//! 与原 PowerShell 实现（一侧断开则关闭两侧）保持一致；字节数在每次读写后
//! 立即累加到原子计数，因此长连接也能反映实时速率。

use crate::config::Config;
use crate::logbuf::LogSink;
use crate::outbound;
use crate::socks5;
use crate::stats::Stats;
use serde::Serialize;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

/// 供 UI 展示的引擎状态。
#[derive(Debug, Clone, Serialize)]
pub struct EngineStatus {
    pub running: bool,
    pub addr: String,
    pub upstream: String,
    pub chain: String,
}

/// 转发缓冲区大小：32 KiB 在吞吐与内存占用间取平衡。
const BUF_SIZE: usize = 32 * 1024;

pub struct EngineHandle {
    stop: Arc<Notify>,
    running: Arc<AtomicBool>,
    task: tokio::task::JoinHandle<()>,
    addr: String,
    upstream: String,
    chain: String,
}

impl EngineHandle {
    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn status(&self) -> EngineStatus {
        EngineStatus {
            running: self.is_running(),
            addr: self.addr.clone(),
            upstream: self.upstream.clone(),
            chain: self.chain.clone(),
        }
    }

    /// 请求停止并等待监听循环退出。
    pub async fn shutdown(self) {
        self.stop.notify_waiters();
        let _ = self.task.await;
    }
}

/// 启动引擎。返回的句柄负责停止与状态查询。
pub async fn start(cfg: Config, logs: LogSink, stats: Stats) -> io::Result<EngineHandle> {
    let listen = cfg.listen_addr();
    let listener = TcpListener::bind(&listen).await?;
    let addr = listener
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or(listen);
    let upstream = cfg.upstream_addr();
    let chain = if cfg.chain_enabled && !cfg.chain_addr.trim().is_empty() {
        cfg.chain_addr.trim().to_string()
    } else {
        "直连".to_string()
    };

    let stop = Arc::new(Notify::new());
    let running = Arc::new(AtomicBool::new(true));
    let shared = Arc::new(cfg);

    let banner = format!("engine 已启动：{addr} → {upstream}（{chain}）");

    let task = {
        let stop = stop.clone();
        let running = running.clone();
        let logs = logs.clone();
        let stats = stats.clone();
        tokio::spawn(async move {
            logs.info(banner);
            loop {
                tokio::select! {
                    _ = stop.notified() => break,
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((sock, _peer)) => {
                                let max = shared.max_conns;
                                if max > 0 && stats.conns() >= max as i64 {
                                    logs.warn(format!("并发已达上限 {max}，拒绝新连接"));
                                    continue;
                                }
                                let cfg = shared.clone();
                                let logs = logs.clone();
                                let stats = stats.clone();
                                tokio::spawn(async move { handle(sock, cfg, logs, stats).await });
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
        upstream,
        chain,
    })
}

async fn handle(mut client: TcpStream, cfg: Arc<Config>, logs: LogSink, stats: Stats) {
    if cfg.tcp_nodelay {
        let _ = client.set_nodelay(true);
    }

    let target = match socks5::handshake(&mut client).await {
        Ok(t) => t,
        Err(e) => {
            logs.debug(format!("SOCKS5 握手失败: {e}"));
            return;
        }
    };
    let authority = target.authority();
    let upstream = cfg.upstream_addr();

    let started = std::time::Instant::now();
    let up = match outbound::connect_tunnel(&cfg, &upstream, &authority).await {
        Ok(s) => s,
        Err(e) => {
            logs.warn(format!("CONNECT {authority} → {e}"));
            let _ = socks5::reply(&mut client, socks5::REP_CONN_REFUSED).await;
            return;
        }
    };

    let rtt = started.elapsed().as_millis();
    if socks5::reply(&mut client, socks5::REP_OK).await.is_err() {
        return;
    }
    logs.info(format!("CONNECT {authority} → OK ({rtt}ms)"));
    stats.inc_conns();

    let (client_r, client_w) = tokio::io::split(client);
    let (up_r, up_w) = tokio::io::split(up);

    let s_up = stats.clone();
    let s_down = stats.clone();
    tokio::select! {
        _ = pump(client_r, up_w, s_up, true) => {},
        _ = pump(up_r, client_w, s_down, false) => {},
    }

    stats.dec_conns();
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
