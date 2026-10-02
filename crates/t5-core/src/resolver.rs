//! 域名解析与 GeoIP 查询。
//!
//! # 为什么要做多地域查询
//!
//! `cloudnproxy.baidu.com` 是 GeoDNS 域名：权威 DNS 按**查询来源**返回就近节点，
//! 因此任何一个解析视角最多只能看到 1~3 个 IP（本地默认 DNS 往往只有 3 个）。
//! 想把各地节点全部拿到，唯一标准手段是 **EDNS Client Subnet（ECS）** ——
//! 在查询里声明「我来自北京电信 / 上海联通 / 广州移动……」，权威 DNS 就会按
//! 对应地域返回该地区节点。阿里、腾讯、Google 的公共 DoH 都免费支持 ECS。
//!
//! 因此解析分两步：
//! 1. **常规多源**：阿里 / 腾讯 / Google DoH + 系统解析并发，取并集；
//! 2. **ECS 多地域**：用一批代表性省网段逐个查询，把各地 CDN 节点「问」出来。
//!
//! 全部结果去重后返回，并附带每个来源贡献了多少个新 IP，便于排查。
//!
//! # 实测结论（2026-10 复核）
//!
//! | 来源 | 常规查询 | ECS 参数 |
//! |---|---|---|
//! | 阿里 `dns.alidns.com` | ✅ | ✅ 按地域返回不同节点（电信/联通尤其准） |
//! | 腾讯 `doh.pub` | ✅ | ❌ 返回 HTTP 400，不支持该参数 |
//! | Google `dns.google` | ✅ | 参数被忽略，但它一次就能返回**全部** A 记录 |
//!
//! 三者互补：Google 通就一把梭，不通则由阿里 ECS 兜底。

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::net::ToSocketAddrs;
use std::time::Duration;

const DOH_ALI: &str = "https://dns.alidns.com/resolve";
const DOH_TENCENT: &str = "https://doh.pub/dns-query";
const DOH_GOOGLE: &str = "https://dns.google/resolve";

/// 代表性客户端子网。命中哪一段就会拿到该地域的节点，
/// 覆盖三大运营商的主要城市即可拿到绝大部分节点。
const ECS_SUBNETS: &[(&str, &str)] = &[
    ("北京电信", "1.202.0.0/24"),
    ("上海电信", "101.226.0.0/24"),
    ("广州电信", "14.215.0.0/24"),
    ("成都电信", "171.208.0.0/24"),
    ("北京联通", "123.116.0.0/24"),
    ("上海联通", "112.64.0.0/24"),
    ("广州联通", "163.177.0.0/24"),
    ("北京移动", "111.13.0.0/24"),
    ("上海移动", "117.131.0.0/24"),
    ("广州移动", "183.232.0.0/24"),
];

/// ip-api.com 的字段选择串（bench 模块拼接 URL 时复用）。
pub const GEO_FIELDS: &str = "status,country,regionName,city,isp,as,query";

/// 单个解析来源的贡献。
#[derive(Debug, Clone, Serialize, Default)]
pub struct ResolveSource {
    pub name: String,
    /// 该来源贡献的**新增** IP 数（与前面来源重复的不计）
    pub count: usize,
}

/// 一次完整解析的结果。
#[derive(Debug, Clone, Serialize, Default)]
pub struct ResolveReport {
    pub ips: Vec<String>,
    pub sources: Vec<ResolveSource>,
}

impl ResolveReport {
    /// 形如 `阿里 DoH:3 腾讯 DoH:2 ECS 北京电信:1`，用于日志。
    pub fn summary(&self) -> String {
        let parts: Vec<String> = self
            .sources
            .iter()
            .filter(|s| s.count > 0)
            .map(|s| format!("{}:{}", s.name, s.count))
            .collect();
        if parts.is_empty() {
            "无".to_string()
        } else {
            parts.join(" ")
        }
    }
}

/// 构造用于直连查询的 HTTP 客户端（rustls，不依赖系统 OpenSSL）。
pub fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(6))
        .user_agent("CloudNProxy/0.1")
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

/// 完整解析：常规多源 + 多地域 ECS。
pub async fn resolve_domain(client: &reqwest::Client, domain: &str) -> ResolveReport {
    let mut report = ResolveReport::default();
    let mut seen: HashSet<String> = HashSet::new();

    // ---- 第一步：常规多源并发 ----
    let (ali, tencent, google) = tokio::join!(
        query_doh(client, DOH_ALI, domain, None),
        query_doh(client, DOH_TENCENT, domain, None),
        query_doh(client, DOH_GOOGLE, domain, None),
    );
    let sys = system_resolve(domain).await;

    for (name, list) in [
        ("阿里 DoH", ali),
        ("腾讯 DoH", tencent),
        ("Google DoH", google),
        ("系统 DNS", sys),
    ] {
        let count = take_new(&mut report.ips, &mut seen, list);
        report.sources.push(ResolveSource {
            name: name.to_string(),
            count,
        });
    }

    // ---- 第二步：ECS 多地域并发 ----
    let mut set = tokio::task::JoinSet::new();
    for (name, subnet) in ECS_SUBNETS {
        let client = client.clone();
        let domain = domain.to_string();
        let label = format!("ECS {name}");
        let subnet = (*subnet).to_string();
        set.spawn(async move {
            let ips = query_doh(&client, DOH_ALI, &domain, Some(&subnet)).await;
            (label, ips)
        });
    }
    while let Some(joined) = set.join_next().await {
        if let Ok((name, list)) = joined {
            let count = take_new(&mut report.ips, &mut seen, list);
            if count > 0 {
                report.sources.push(ResolveSource { name, count });
            }
        }
    }

    report
        .ips
        .sort_by_key(|ip| match ip.parse::<std::net::Ipv4Addr>() {
            Ok(v4) => (0u8, u32::from(v4)),
            Err(_) => (1u8, 0u32),
        });
    report
}

/// 把新出现的 IP 追加到 `out`，返回新增数量。
fn take_new(out: &mut Vec<String>, seen: &mut HashSet<String>, list: Vec<String>) -> usize {
    let mut added = 0;
    for ip in list {
        if is_usable_ipv4(&ip) && seen.insert(ip.clone()) {
            out.push(ip);
            added += 1;
        }
    }
    added
}

/// 过滤掉保留网段：某些本地代理/劫持会返回 `198.18.0.0/15` 之类的假地址，
/// 它们不是真实节点，参与测速只会浪费时间。
fn is_usable_ipv4(ip: &str) -> bool {
    match ip.parse::<std::net::Ipv4Addr>() {
        Ok(v4) => {
            let o = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_documentation()
                // 198.18.0.0/15：RFC 2544 网络设备基准测试段
                || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
                // 100.64.0.0/10：运营商级 NAT
                || (o[0] == 100 && (64..128).contains(&o[1]))
                // 192.0.0.0/24、192.0.2.0/24 等
                || (o[0] == 192 && o[1] == 0 && o[2] == 0))
        }
        Err(_) => false,
    }
}

async fn query_doh(
    client: &reqwest::Client,
    base: &str,
    domain: &str,
    ecs: Option<&str>,
) -> Vec<String> {
    let mut url = format!("{base}?name={domain}&type=A");
    if let Some(subnet) = ecs {
        // 斜杠编码成 %2F：实测阿里两种写法都接受，但编码后能避免中间设备
        // 或网关把 `1.202.0.0/24` 里的斜杠误当作路径分隔符。
        url.push_str("&edns_client_subnet=");
        url.push_str(&subnet.replace('/', "%2F"));
    }

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

async fn system_resolve(domain: &str) -> Vec<String> {
    let d = domain.to_string();
    tokio::task::spawn_blocking(move || {
        format!("{d}:443")
            .to_socket_addrs()
            .map(|it| it.map(|a| a.ip().to_string()).collect::<Vec<_>>())
            .unwrap_or_default()
    })
    .await
    .unwrap_or_default()
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

    #[test]
    fn filters_reserved_addresses() {
        // 真实公网地址
        assert!(is_usable_ipv4("14.215.182.75"));
        assert!(is_usable_ipv4("163.177.17.189"));
        // RFC 2544 基准测试段（本地代理/hijack 常见返回值）
        assert!(!is_usable_ipv4("198.19.207.203"));
        assert!(!is_usable_ipv4("198.18.0.1"));
        // 私有与回环
        assert!(!is_usable_ipv4("10.0.0.1"));
        assert!(!is_usable_ipv4("192.168.1.1"));
        assert!(!is_usable_ipv4("127.0.0.1"));
    }

    #[test]
    fn dedups_across_sources() {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let n1 = take_new(
            &mut out,
            &mut seen,
            vec!["1.1.1.1".into(), "2.2.2.2".into()],
        );
        let n2 = take_new(
            &mut out,
            &mut seen,
            vec!["2.2.2.2".into(), "3.3.3.3".into()],
        );
        assert_eq!(n1, 2);
        assert_eq!(n2, 1);
        assert_eq!(out.len(), 3);
    }
}
