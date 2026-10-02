//! 节点测速：延迟、真实下载带宽、出口 IP 与归属。
//!
//! # 测速链接可配置
//!
//! 链接取自配置的 `speed_url`（界面「节点库」页可下拉选择或自行添加），
//! 默认是 `https://speed.cloudflare.com/__down?bytes=1000000`。换用更大的
//! `bytes` 可以得到更稳定的结果，代价是耗时更长。
//!
//! # 为什么走 HTTPS
//!
//! `speed.cloudflare.com` 已拒绝对 `/__down` 的**明文 HTTP** 访问
//! （返回 403 Forbidden），因此必须走 HTTPS。隧道本身只做裸 TCP，
//! 为了完成 TLS，这里用 [`crate::temp_proxy`] 起一个只服务被测节点的
//! 临时 SOCKS5，再让 `reqwest` 通过它请求 —— TLS 由 reqwest 处理，
//! 不需要为测速单独引入 TLS 依赖。
//!
//! 出口 IP 与归属仍走隧道内的明文 HTTP（`ip-api.com` 免费接口本身只提供 HTTP）。

use crate::config::Config;
use crate::outbound;
use crate::resolver::{self, GeoInfo};
use crate::temp_proxy;
use serde::Serialize;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// 单次测速的硬上限：慢节点不会把整个批量测速拖住。
/// 到达上限时按「已下载字节 ÷ 已耗时」计算速度，结果同样有效。
const SPEED_MAX_SECS: u64 = 30;

const GEO_HOST: &str = "ip-api.com";
const MAX_BODY: usize = 1 << 20;

#[derive(Debug, Clone, Serialize, Default)]
pub struct BenchResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed_mbps: Option<f64>,
    /// 实际下载到的字节数
    pub bytes: u64,
    /// 下载耗时（秒）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seconds: Option<f64>,
    /// 实际使用的测速链接
    #[serde(skip_serializing_if = "String::is_empty")]
    pub url: String,
    /// 节点自身归属地
    pub region: String,
    /// 节点自身运营商
    pub entry_isp: String,
    /// 出口 IP
    pub exit_ip: String,
    /// 出口归属地
    pub exit_region: String,
    /// 出口运营商
    pub exit_isp: String,
    /// 出口 ASN
    pub exit_asn: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 对一个节点做完整测速。测速链接取自 `cfg.effective_speed_url()`。
pub async fn benchmark(cfg: &Config, node_addr: &str, client: &reqwest::Client) -> BenchResult {
    let mut result = BenchResult::default();
    result.url = cfg.effective_speed_url().to_string();

    match speed_test(cfg, node_addr).await {
        Ok((latency, mbps, bytes, seconds)) => {
            result.latency_ms = Some(latency);
            result.speed_mbps = Some(round2(mbps));
            result.bytes = bytes;
            result.seconds = Some(round2(seconds));
        }
        Err(e) => result.error = Some(e.to_string()),
    }

    if let Some(g) = exit_geo(cfg, node_addr).await {
        result.exit_ip = g.query.clone();
        result.exit_region = g.place();
        result.exit_isp = g.isp_class();
        result.exit_asn = g.asn_short();
    }

    let node_ip = node_addr.split(':').next().unwrap_or(node_addr);
    if let Some(g) = resolver::geo_direct(client, node_ip).await {
        result.region = g.place();
        result.entry_isp = g.isp_class();
    }

    result
}

/// 返回 (CONNECT 往返毫秒, Mbps, 下载字节数, 下载秒数)。
async fn speed_test(cfg: &Config, node_addr: &str) -> io::Result<(u32, f64, u64, f64)> {
    let url = cfg.effective_speed_url().to_string();
    let (host, port) = host_port(&url)?;
    let cfg = Arc::new(cfg.clone());

    // 1) 先用一次隧道建立量出 CONNECT 往返，同时确认节点可用
    let probe_target = format!("{host}:{port}");
    let t0 = Instant::now();
    let probe = outbound::connect_tunnel(&cfg, node_addr, &probe_target).await?;
    let latency = t0.elapsed().as_millis() as u32;
    drop(probe);

    // 2) 起临时 SOCKS5，让 reqwest 通过它走 HTTPS 下载
    let proxy = temp_proxy::spawn(Arc::clone(&cfg), node_addr.to_string(), 4).await?;
    let result = download_via_proxy(&proxy.addr, &url).await;
    proxy.shutdown().await;

    result.map(|(mbps, bytes, secs)| (latency, mbps, bytes, secs))
}

async fn download_via_proxy(proxy_addr: &str, url: &str) -> io::Result<(f64, u64, f64)> {
    let proxy = reqwest::Proxy::all(format!("socks5h://{proxy_addr}"))
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("代理配置失败: {e}")))?;

    let client = reqwest::Client::builder()
        .proxy(proxy)
        .timeout(Duration::from_secs(SPEED_MAX_SECS + 15))
        .user_agent("CloudNProxy/0.1")
        .build()
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("HTTP 客户端构建失败: {e}")))?;

    let start = Instant::now();
    let mut resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("请求失败: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("测速链接返回 HTTP {status}"),
        ));
    }

    let deadline = Duration::from_secs(SPEED_MAX_SECS);
    let mut total: u64 = 0;
    loop {
        let elapsed = start.elapsed();
        if elapsed >= deadline {
            break;
        }
        match tokio::time::timeout(deadline.saturating_sub(elapsed), resp.chunk()).await {
            Ok(Ok(Some(chunk))) => total += chunk.len() as u64,
            Ok(Ok(None)) => break,
            Ok(Err(e)) => {
                if total == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        format!("读取响应失败: {e}"),
                    ));
                }
                break;
            }
            Err(_) => break,
        }
    }

    let secs = start.elapsed().as_secs_f64();
    if total == 0 || secs <= 0.0 {
        return Err(io::Error::new(io::ErrorKind::Other, "测速无数据返回"));
    }
    let mbps = (total as f64 * 8.0) / secs / 1_000_000.0;
    Ok((mbps, total, secs))
}

/// 从 URL 提取主机与端口，用于建立隧道。端口缺省时按 scheme 推断。
fn host_port(url: &str) -> io::Result<(String, u16)> {
    let rest = url.split("://").nth(1).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("测速链接缺少 http:// 或 https:// 前缀: {url}"),
        )
    })?;
    let default_port = if url.starts_with("https://") { 443 } else { 80 };

    let authority = rest
        .split(|c| c == '/' || c == '?' || c == '#')
        .next()
        .unwrap_or(rest);

    if authority.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("测速链接缺少主机名: {url}"),
        ));
    }

    // IPv6 字面量：形如 [2400:3200::1]:443
    if let Some(close) = authority.find(']') {
        let host = authority[1..close].to_string();
        let port = match authority[close + 1..].strip_prefix(':') {
            Some(p) => p.parse::<u16>().map_err(|_| bad_port(authority))?,
            None => default_port,
        };
        if host.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("测速链接缺少主机名: {url}"),
            ));
        }
        return Ok((host, port));
    }

    match authority.rsplit_once(':') {
        Some((h, p)) => {
            if h.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("测速链接缺少主机名: {url}"),
                ));
            }
            Ok((h.to_string(), p.parse::<u16>().map_err(|_| bad_port(authority))?))
        }
        None => Ok((authority.to_string(), default_port)),
    }
}

fn bad_port(authority: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("测速链接端口无效: {authority}"),
    )
}

/// 经隧道请求 ip-api.com，一次拿到出口 IP 及其归属。
async fn exit_geo(cfg: &Config, node_addr: &str) -> Option<GeoInfo> {
    let target = format!("{GEO_HOST}:80");
    let mut s = outbound::connect_tunnel(cfg, node_addr, &target).await.ok()?;
    let path = format!("/json/?fields={}", resolver::GEO_FIELDS);
    let body = http_get_through(&mut s, GEO_HOST, &path).await.ok()?;
    let info: GeoInfo = serde_json::from_str(&body).ok()?;
    if info.status == "fail" {
        None
    } else {
        Some(info)
    }
}

/// 在已有隧道上发一次简单 GET，返回去掉响应头后的正文。
async fn http_get_through<S>(s: &mut S, host: &str, path: &str) -> io::Result<String>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\n\
         User-Agent: CloudNProxy/0.1\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
    s.write_all(req.as_bytes()).await?;

    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut tmp = [0u8; 8192];
    loop {
        let n = match s.read(&mut tmp).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > MAX_BODY {
            break;
        }
    }

    let text = String::from_utf8_lossy(&buf).to_string();
    Ok(match text.find("\r\n\r\n") {
        Some(i) => text[i + 4..].to_string(),
        None => text,
    })
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_to_two_decimals() {
        assert_eq!(round2(12.3456), 12.35);
        assert_eq!(round2(0.0), 0.0);
    }

    #[test]
    fn parses_https_url_with_default_port() {
        let (host, port) = host_port("https://speed.cloudflare.com/__down?bytes=1000").unwrap();
        assert_eq!(host, "speed.cloudflare.com");
        assert_eq!(port, 443);
    }

    #[test]
    fn parses_http_url_with_explicit_port() {
        let (host, port) = host_port("http://example.com:8080/path").unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 8080);
    }

    #[test]
    fn parses_url_without_path() {
        let (host, port) = host_port("http://cachefly.cachefly.net").unwrap();
        assert_eq!(host, "cachefly.cachefly.net");
        assert_eq!(port, 80);
    }

    #[test]
    fn rejects_invalid_urls() {
        assert!(host_port("speed.cloudflare.com/__down").is_err());
        assert!(host_port("https://").is_err());
        assert!(host_port("http://example.com:abc/").is_err());
    }
}
