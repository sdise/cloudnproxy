//! 配置结构与 TOML 读写。
//!
//! 配置文件默认落点由调用方给出（Tauri 侧为 app_config_dir/config.toml）。
//! 所有 `Option` 字段都带 `skip_serializing_if`，因为 TOML 不支持 `None` 值。

use serde::{Deserialize, Serialize};
use std::path::Path;

fn default_port() -> u16 {
    443
}

/// 一个候选 T5 节点及其最近一次探测结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Node {
    /// 节点 IP
    pub ip: String,
    /// 节点端口，默认 443
    #[serde(default = "default_port")]
    pub port: u16,
    /// 节点归属地城市
    pub region: String,
    /// 节点所在运营商（已归一化为 电信/联通/移动）
    pub entry_isp: String,
    /// 探到的出口 IP
    pub exit_ip: String,
    /// 出口归属地
    pub exit_region: String,
    /// 出口运营商
    pub exit_isp: String,
    /// 出口 ASN，形如 `AS4134`
    pub exit_asn: String,
    /// 最近一次 CONNECT 往返耗时
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u32>,
    /// 最近一次下行速度
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed_mbps: Option<f64>,
    /// 最近一次测速的 Unix 时间戳（秒）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measured_at: Option<u64>,
}

impl Default for Node {
    fn default() -> Self {
        Self {
            ip: String::new(),
            port: 443,
            region: String::new(),
            entry_isp: String::new(),
            exit_ip: String::new(),
            exit_region: String::new(),
            exit_isp: String::new(),
            exit_asn: String::new(),
            latency_ms: None,
            speed_mbps: None,
            measured_at: None,
        }
    }
}

impl Node {
    pub fn new(ip: impl Into<String>) -> Self {
        Self {
            ip: ip.into(),
            ..Default::default()
        }
    }

    /// `ip:port` 形式的上游地址。
    pub fn addr(&self) -> String {
        format!("{}:{}", self.ip, self.port)
    }

    /// 是否已成功连通过（有延迟或速度记录）。
    pub fn usable(&self) -> bool {
        self.latency_ms.is_some() || self.speed_mbps.is_some()
    }
}

/// 应用全部可配置项。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    // ---- 入站 ----
    pub listen_host: String,
    pub listen_port: u16,
    pub allow_lan: bool,

    // ---- 上游 T5 ----
    /// 节点解析来源域名
    pub resolve_domain: String,
    /// 当前使用的上游节点，`ip:port`
    pub upstream: String,
    /// 注入的伪 Host
    pub fake_host: String,
    /// 注入的 X-T5-Auth
    pub t5_auth: String,
    /// 最大并发连接数，0 表示不限制
    pub max_conns: u32,

    // ---- Chain 一级代理 ----
    pub chain_enabled: bool,
    pub chain_addr: String,

    // ---- 转发行为 ----
    pub connect_timeout_ms: u64,
    pub tcp_nodelay: bool,
    pub tunnel_pool: bool,
    pub auto_reconnect: bool,
    pub auto_switch: bool,

    // ---- 应用行为 ----
    pub autostart: bool,
    pub autostart_connect: bool,
    pub start_minimized: bool,
    pub close_to_tray: bool,
    pub floating: bool,
    pub theme: String,

    // ---- 数据 ----
    /// 当前节点的 `ip:port`，与 `upstream` 保持一致
    pub current_node: String,
    pub nodes: Vec<Node>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_host: "127.0.0.1".to_string(),
            listen_port: 10801,
            allow_lan: false,

            resolve_domain: "cloudnproxy.baidu.com".to_string(),
            upstream: "163.177.17.189:443".to_string(),
            fake_host: "cloudnproxy.baidu.com".to_string(),
            t5_auth: "1050504963".to_string(),
            max_conns: 512,

            chain_enabled: false,
            chain_addr: String::new(),

            connect_timeout_ms: 10_000,
            tcp_nodelay: true,
            tunnel_pool: true,
            auto_reconnect: true,
            auto_switch: false,

            autostart: false,
            autostart_connect: true,
            start_minimized: true,
            close_to_tray: true,
            floating: false,
            theme: "system".to_string(),

            current_node: String::new(),
            nodes: Vec::new(),
        }
    }
}

impl Config {
    /// 入站监听地址；开启「允许局域网」时绑定 0.0.0.0。
    pub fn listen_addr(&self) -> String {
        let host = if self.allow_lan {
            "0.0.0.0"
        } else {
            self.listen_host.as_str()
        };
        format!("{host}:{}", self.listen_port)
    }

    /// 实际使用的上游节点地址。
    pub fn upstream_addr(&self) -> String {
        if !self.current_node.trim().is_empty() {
            self.current_node.trim().to_string()
        } else {
            self.upstream.trim().to_string()
        }
    }

    /// 用一批 IP 更新节点列表：保留已有节点的探测结果，新增缺失的，删除已消失的。
    pub fn merge_node_ips(&mut self, ips: &[String]) {
        let mut kept: Vec<Node> = Vec::with_capacity(ips.len());
        for ip in ips {
            match self.nodes.iter().find(|n| &n.ip == ip) {
                Some(existing) => kept.push(existing.clone()),
                None => kept.push(Node::new(ip.clone())),
            }
        }
        self.nodes = kept;
    }

    pub fn load(path: &Path) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        toml::from_str(&text).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, format!("config 解析失败: {e}"))
        })
    }

    /// 读取配置；文件不存在时返回默认配置。
    pub fn load_or_default(path: &Path) -> Self {
        if path.exists() {
            Self::load(path).unwrap_or_default()
        } else {
            Self::default()
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, format!("config 序列化失败: {e}"))
        })?;
        std::fs::write(path, text)
    }
}
