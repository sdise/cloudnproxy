//! 出站：与 T5 节点建立隧道。
//!
//! 关键动作是把标准 `CONNECT` 请求改写为 T5 节点能识别的形式：
//!
//! ```text
//! CONNECT <真实目标host:port> HTTP/1.1
//! Host: cloudnproxy.baidu.com     ← 伪 Host，不是真实域名
//! X-T5-Auth: <凭证>
//! Proxy-Connection: Keep-Alive
//! ```
//!
//! 若配置了 Chain（一级 HTTP 代理），则先连到 Chain 并要求它 CONNECT 到节点，
//! 再在这条链路里发起上面的改写请求，形成三级链路。

use crate::config::Config;
use std::io;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// 去掉可能造成 HTTP 头注入的换行符。
fn sanitize(v: &str) -> String {
    v.replace('\r', "").replace('\n', "")
}

/// 建立到 `target_authority` 的隧道并返回已就绪的流。
pub async fn connect_tunnel(
    cfg: &Config,
    node_addr: &str,
    target_authority: &str,
) -> io::Result<TcpStream> {
    let timeout = Duration::from_millis(cfg.connect_timeout_ms.max(1_000));
    match tokio::time::timeout(timeout, open(cfg, node_addr, target_authority)).await {
        Ok(r) => r,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("连接超时（{}ms）", cfg.connect_timeout_ms),
        )),
    }
}

async fn open(cfg: &Config, node_addr: &str, target_authority: &str) -> io::Result<TcpStream> {
    // ---- 第一步：连到节点；若启用 Chain 则先过一级代理 ----
    let mut stream = if cfg.chain_enabled && !cfg.chain_addr.trim().is_empty() {
        let chain = cfg.chain_addr.trim();
        let mut s = TcpStream::connect(chain).await?;
        let _ = s.set_nodelay(true);
        let req = format!("CONNECT {node_addr} HTTP/1.1\r\nHost: {node_addr}\r\n\r\n");
        s.write_all(req.as_bytes()).await?;
        let head = read_headers(&mut s).await?;
        if !status_ok(&head) {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("chain CONNECT 失败: {}", first_line(&head)),
            ));
        }
        s
    } else {
        TcpStream::connect(node_addr).await?
    };

    if cfg.tcp_nodelay {
        let _ = stream.set_nodelay(true);
    }

    // ---- 第二步：发改写后的 CONNECT ----
    let host = sanitize(&cfg.fake_host);
    let auth = sanitize(&cfg.t5_auth);
    let mut req = format!("CONNECT {target_authority} HTTP/1.1\r\nHost: {host}\r\n");
    if !auth.trim().is_empty() {
        req.push_str(&format!("X-T5-Auth: {}\r\n", auth.trim()));
    }
    req.push_str("Proxy-Connection: Keep-Alive\r\n\r\n");
    stream.write_all(req.as_bytes()).await?;

    let head = read_headers(&mut stream).await?;
    if !status_ok(&head) {
        return Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!("DENIED {}", first_line(&head)),
        ));
    }

    Ok(stream)
}

/// 逐字节读取直到 `\r\n\r\n`，返回完整响应头文本。
pub async fn read_headers<S>(s: &mut S) -> io::Result<String>
where
    S: AsyncRead + Unpin,
{
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    let mut one = [0u8; 1];
    loop {
        let n = s.read(&mut one).await?;
        if n == 0 {
            break;
        }
        buf.push(one[0]);
        if buf.len() >= 4 && &buf[buf.len() - 4..] == b"\r\n\r\n" {
            break;
        }
        if buf.len() > 8192 {
            break;
        }
    }
    Ok(String::from_utf8_lossy(&buf).to_string())
}

/// 状态行是否为 200。
pub fn status_ok(head: &str) -> bool {
    first_line(head).contains(" 200")
}

/// 取响应首行。
pub fn first_line(head: &str) -> String {
    head.split("\r\n").next().unwrap_or("").trim().to_string()
}
