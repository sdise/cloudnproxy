//! 临时单节点 SOCKS5 转发服务。
//!
//! 用途只有一个：**测速**。
//!
//! 转发引擎本身只做裸 TCP 隧道，不带 TLS；而 `speed.cloudflare.com` 已经
//! 拒绝明文 HTTP 访问 `/__down`（返回 403），必须走 HTTPS。与其为测速单独
//! 引入一套 TLS 实现（还要处理与 reqwest 的 crypto provider 冲突、musl 静态
//! 编译问题），不如让 `reqwest` 自己完成 TLS —— 只需要给它一个 SOCKS5 出口。
//!
//! 于是这里在 `127.0.0.1` 的随机端口上起一个**只服务指定节点**的临时代理：
//! 它接受 SOCKS5 请求，用配置里的节点建立隧道，然后双向转发。测速结束后
//! 立即关闭，不占用固定端口，也不参与正常代理流程。

use crate::config::Config;
use crate::outbound;
use crate::socks5;
use std::io;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

const BUF_SIZE: usize = 64 * 1024;

pub struct TempProxy {
    /// 形如 `127.0.0.1:54321`，可直接拼成 `socks5h://...`
    pub addr: String,
    stop: Arc<Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl TempProxy {
    /// 关闭监听并等待任务退出。
    pub async fn shutdown(self) {
        self.stop.notify_waiters();
        let _ = self.task.await;
    }
}

/// 启动临时代理。`max_conns` 为 0 表示不限制（由调用方保证及时 shutdown）。
pub async fn spawn(
    cfg: Arc<Config>,
    node_addr: String,
    max_conns: usize,
) -> io::Result<TempProxy> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?.to_string();
    let stop = Arc::new(Notify::new());

    let task = {
        let stop = Arc::clone(&stop);
        tokio::spawn(async move {
            let mut served = 0usize;
            loop {
                tokio::select! {
                    _ = stop.notified() => break,
                    accepted = listener.accept() => {
                        let sock = match accepted {
                            Ok((s, _)) => s,
                            Err(_) => continue,
                        };
                        served += 1;
                        let cfg = Arc::clone(&cfg);
                        let node = node_addr.clone();
                        tokio::spawn(async move { serve(sock, cfg, node).await });
                        if max_conns > 0 && served >= max_conns {
                            break;
                        }
                    }
                }
            }
        })
    };

    Ok(TempProxy { addr, stop, task })
}

async fn serve(mut client: TcpStream, cfg: Arc<Config>, node: String) {
    let target = match socks5::handshake(&mut client).await {
        Ok(t) => t,
        Err(_) => return,
    };

    let authority = target.authority();
    match outbound::connect_tunnel(&cfg, &node, &authority).await {
        Ok(up) => {
            if socks5::reply(&mut client, socks5::REP_OK).await.is_err() {
                return;
            }
            let (client_r, client_w) = tokio::io::split(client);
            let (up_r, up_w) = tokio::io::split(up);
            tokio::select! {
                _ = copy(client_r, up_w) => {},
                _ = copy(up_r, client_w) => {},
            }
        }
        Err(_) => {
            let _ = socks5::reply(&mut client, socks5::REP_CONN_REFUSED).await;
        }
    }
}

async fn copy<R, W>(mut r: R, mut w: W)
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
    }
    let _ = w.shutdown().await;
}
