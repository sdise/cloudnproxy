//! 域名解析与 GeoIP 查询。
//!
//! 节点来源是 `cloudnproxy.baidu.com` 的 DNS 解析结果：并发查询多个 DoH 源
//! 取并集，再用系统解析兜底。GeoIP 使用 ip-api.com 的明文 HTTP 接口，
//! 便于在隧道内直接发起（无需 TLS）。

use serde::Deserialize;
use std::net::ToSocketAddrs;
use std::time::Duration;

const DOH_ALI: &str = "https://dns.alidns.com/resolve";
const DOH_TENCENT: &str = "https://doh.pub/dns-query";

/// ip-api.com 的字段选择串（同时被 bench 模块拼接 URL 使用）。
pub const GEO_FIELDS: &str = "status,country,regionName,city,isp,as,query";

/// 构造用于直连查询的 HTTP 客户端（rustls，不依赖系统 OpenSSL）。
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(6))
        .user_agent("CloudNProxy/1.0")
        .build()
        .unwrap_or_default()
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct DohResp {
    #[serde(rename = "Answer")]
    pub answer: Vec<DohAnswer>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
pub struct DohAnswer {
    #[serde(rename = "type")]
    pub rtype: u16,
    pub data: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct GeoInfo {
    pub status: String,
    pub query: String,
    pub country: String,
    #[serde(rename = "regionName")]
    pub region_name: String,
    pub city: String,
    pub isp: String,
    /// 形如 `AS4134 CHINANET-BACKBONE`
    #[serde(rename = "as")]
    pub asn: String,
}

impl GeoInfo {
    /// 优先城市，退回省份。
    pub fn place(&self) -> String {
        if !self.city.is_empty() {
            self.city.clone()
        } else {
            self.region_name.clone()
        }
    }

    /// 取 ASN 前缀，如 `AS4134`。
    pub fn asn_short(&self) -> String {
        self.asn.split_whitespace().next().unwrap_or("").to_string()
    }

    /// 归一化后的运营商名。
    pub fn isp_class(&self) -> String {
        classify_isp(&self.isp)
    }
}

/// 解析域名，返回去重排序后的 IPv4 列表。
pub async fn resolve_domain(client: &reqwest::Client, domain: &str) -> Vec<String> {
    let (a, b) = tokio::join!(
        query_doh(client, DOH_ALI, domain),
        query_doh(client, DOH_TENCENT, domain)
    );

    let mut ips: Vec<String> = Vec::new();
    for ip in a.into_iter().chain(b.into_iter()) {
        if !ips.contains(&ip) {
            ips.push(ip);
        }
    }

    // 系统解析兜底
    let d = domain.to_string();
    let sys = tokio::task::spawn_blocking(move || {
        format!("{d}:443")
            .to_socket_addrs()
            .map(|it| it.map(|a| a.ip().to_string()).collect::<Vec<_>>())
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default();

    for ip in sys {
        if !ips.contains(&ip) {
            ips.push(ip);
        }
    }

    ips.sort_by_key(|ip| match ip.parse::<std::net::Ipv4Addr>() {
        Ok(v4) => (0u8, u32::from(v4)),
        Err(_) => (1u8, 0u32),
    });
    ips
}

async fn query_doh(client: &reqwest::Client, base: &str, domain: &str) -> Vec<String> {
    let url = format!("{base}?name={domain}&type=A");
    let resp = match client
        .get(&url)
        .header("accept", "application/dns-json")
        .send()
        .await
    {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    let body = match resp.json::<DohResp>().await {
        Ok(j) => j,
        Err(_) => return Vec::new(),
    };
    body.answer
        .into_iter()
        .filter(|a| a.rtype == 1)
        .map(|a| a.data)
        .collect()
}

/// 直连查询某个 IP 的归属信息（不经过隧道）。
pub async fn geo_direct(client: &reqwest::Client, ip: &str) -> Option<GeoInfo> {
    let url = format!("http://ip-api.com/json/{ip}?fields={GEO_FIELDS}");
    let resp = client.get(&url).send().await.ok()?;
    let info = resp.json::<GeoInfo>().await.ok()?;
    if info.status == "fail" {
        None
    } else {
        Some(info)
    }
}

/// 把运营商原文归一化为 电信 / 联通 / 移动。
pub fn classify_isp(org: &str) -> String {
    if org.trim().is_empty() {
        return String::new();
    }
    let o = org.to_lowercase();
    if o.contains("telecom") || o.contains("chinanet") || o.contains("4134") {
        return "电信".to_string();
    }
    if o.contains("mobile") || o.contains("cmnet") || o.contains("9808") {
        return "移动".to_string();
    }
    if o.contains("unicom") || o.contains("china169") || o.contains("4837") {
        return "联通".to_string();
    }
    org.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_known_operators() {
        assert_eq!(classify_isp("CHINANET-BACKBONE"), "电信");
        assert_eq!(classify_isp("China Unicom"), "联通");
        assert_eq!(classify_isp("China Mobile"), "移动");
        assert_eq!(classify_isp(""), "");
    }

    #[test]
    fn extracts_asn_prefix() {
        let g = GeoInfo {
            asn: "AS4134 CHINANET-BACKBONE".into(),
            ..Default::default()
        };
        assert_eq!(g.asn_short(), "AS4134");
    }
}
