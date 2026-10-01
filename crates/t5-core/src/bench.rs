//! 节点测速：延迟、下行带宽、出口 IP 与归属。
//!
//! 全部通过节点隧道完成（出口信息必须经隧道才有意义），
//! 节点自身的归属则直连查询。测速上限 100 MiB / 30 秒。

use crate::config::Config;
use crate::outbound;
use crate::resolver::{self, GeoInfo};
use serde::Serialize;
use std::io;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const SPEED_HOST: &str = "speed.cloudflare.com";
const SPEED_BYTES: u64 = 100 * 1024 * 1024;
const SPEED_MAX_SECS: u64 = 30;
const GEO_HOST: &str = "ip-api.com";
const MAX_BODY: usize = 1 << 20;

#[derive(Debug, Clone, Serialize, Default)]
pub struct BenchResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed_mbps: Option<f64>,
    pub bytes: u64,
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

/// 对一个节点做完整测速。
pub async fn benchmark(cfg: &Config, node_addr: &str, client: &reqwest::Client) -> BenchResult {
    let mut result = BenchResult::default();

    match speed_test(cfg, node_addr).await {
        Ok((latency, mbps, bytes)) => {
            result.latency_ms = Some(latency);
            result.speed_mbps = Some(round2(mbps));
            result.bytes = bytes;
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

/// 返回 (CONNECT 往返毫秒, Mbps, 实际字节数)。
async fn speed_test(cfg: &Config, node_addr: &str) -> io::Result<(u32, f64, u64)> {
    let target = format!("{SPEED_HOST}:80");
    let t0 = Instant::now();
    let mut s = outbound::connect_tunnel(cfg, node_addr, &target).await?;
    let latency = t0.elapsed().as_millis() as u32;

    let req = format!(
        "GET /__down?bytes={SPEED_BYTES} HTTP/1.1\r\nHost: {SPEED_HOST}\r\n\
         User-Agent: CloudNProxy/1.0\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    );
    s.write_all(req.as_bytes()).await?;

    let head = outbound::read_headers(&mut s).await?;
    if !outbound::status_ok(&head) {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("测速接口返回异常: {}", outbound::first_line(&head)),
        ));
    }

    let start = Instant::now();
    let deadline = Duration::from_secs(SPEED_MAX_SECS);
    let mut total: u64 = 0;
    let mut buf = vec![0u8; 64 * 1024];

    loop {
        let elapsed = start.elapsed();
        if elapsed >= deadline || total >= SPEED_BYTES {
            break;
        }
        let remain = deadline.saturating_sub(elapsed);
        match tokio::time::timeout(remain, s.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => total += n as u64,
            Ok(Err(_)) => break,
            Err(_) => break,
        }
    }

    let secs = start.elapsed().as_secs_f64();
    if total == 0 || secs <= 0.0 {
        return Err(io::Error::new(io::ErrorKind::Other, "测速无数据返回"));
    }
    let mbps = (total as f64 * 8.0) / secs / 1_000_000.0;
    Ok((latency, mbps, total))
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
         User-Agent: CloudNProxy/1.0\r\nAccept: */*\r\nConnection: close\r\n\r\n"
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
