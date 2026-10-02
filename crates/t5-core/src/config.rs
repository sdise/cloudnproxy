//! 配置结构与 TOML 读写。
//!
//! 配置文件默认落点由调用方给出（Tauri 侧为 app_config_dir/config.toml）。
//! 所有 `Option` 字段都带 `skip_serializing_if`，因为 TOML 不支持 `None` 值。

use serde::{Deserialize, Serialize};
use std::path::Path;

fn default_port() -> u16 {
    443
}

/// 默认测速链接：1 MB 固定大小，便于快速采样。
pub const DEFAULT_SPEED_URL: &str = "https://speed.cloudflare.com/__down?bytes=1000000";

/// 内置可选的测速链接。
///
/// 注意 Cloudflare 的 `/__down` 对 `bytes` 有上限（**99 MB**），
/// 写 `100000000` 会被拒绝，因此这里用 99 MB。
fn default_speed_urls() -> Vec<String> {
    vec![
        DEFAULT_SPEED_URL.to_string(),
        "https://speed.cloudflare.com/__down?bytes=10000000".to_string(),
        "https://speed.cloudflare.com/__down?bytes=99000000".to_string(),
    ]
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

/// Web 控制台配置（无 GUI 版本专用）。
///
/// 默认**关闭**：把控制台暴露到网络是个安全决定，应当由使用者显式开启。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WebConfig {
    /// 是否启用 Web 控制台
    pub enabled: bool,
    /// 监听地址。默认监听所有网卡，公网部署时请务必配合防火墙 / 反向代理。
    pub listen: String,
    /// 登录用户名
    pub username: String,
    /// argon2id 密码散列（PHC 字符串）。
    ///
    /// 留空表示尚未初始化，首次启动时会生成随机初始密码并输出到日志一次。
    pub password_hash: String,
    /// 是否必须先修改密码才能使用。首次初始化后为 `true`。
    pub must_change_password: bool,
    /// JWT 签名密钥。留空表示尚未初始化，首次启动时随机生成并写回配置
    /// （持久化是为了让已签发的令牌在重启后仍然有效）。
    pub jwt_secret: String,
    /// 登录令牌有效期（小时）
    pub token_ttl_hours: u64,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: "0.0.0.0:10110".to_string(),
            username: "admin".to_string(),
            password_hash: String::new(),
            must_change_password: false,
            jwt_secret: String::new(),
            token_ttl_hours: 24,
        }
    }
}

impl WebConfig {
    /// 把 `listen` 拆成 `(host, port)`，非法时回退到默认值。
    pub fn listen_parts(&self) -> (String, u16) {
        match self.listen.trim().rsplit_once(':') {
            Some((h, p)) => match p.parse::<u16>() {
                Ok(port) => (h.to_string(), port),
                Err(_) => ("0.0.0.0".to_string(), 10110),
            },
            None => ("0.0.0.0".to_string(), 10110),
        }
    }

    /// 是否绑定了非回环地址（即真的对外可达）。
    pub fn exposed_to_network(&self) -> bool {
        let (host, _) = self.listen_parts();
        let h = host.trim();
        !(h == "127.0.0.1" || h == "localhost" || h == "::1")
    }
}

/// 从节点库中选出评分最高的可用节点，返回 `ip:port`。
///
/// 评分规则：**速度优先、延迟次之**。速度是 Mbps 量级（几十到几百），延迟是
/// 毫秒量级（几十），乘 1000 让速度主导，延迟只作同速时的次要因素。
///
/// `exclude` 用于排除某个地址：自动故障切换的场景下选回自己毫无意义，所以传
/// `Some(当前节点)`；而「测速后自动选优」允许当前节点就是最优，传 `None`。
///
/// 没有任何已测速的节点时返回 `None`。
pub fn pick_best(nodes: &[Node], exclude: Option<&str>) -> Option<String> {
    let mut best: Option<(&Node, f64)> = None;
    for n in nodes {
        let addr = n.addr();
        if exclude == Some(addr.as_str()) {
            continue;
        }
        // 没测过速的节点不参与评分，避免把「未知」当成「最好」
        if n.latency_ms.is_none() && n.speed_mbps.is_none() {
            continue;
        }
        let score =
            n.speed_mbps.unwrap_or(0.0) * 1000.0 - n.latency_ms.unwrap_or(9_999) as f64;
        if best.map(|(_, s)| score > s).unwrap_or(true) {
            best = Some((n, score));
        }
    }
    best.map(|(n, _)| n.addr())
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
    /// 出站网卡绑定：
    /// - 空字符串：自动选择物理网卡（绕过 TUN，默认）
    /// - `system`：不绑定，完全跟随系统路由表（可能被 TUN 接管）
    /// - 其他：按名称绑定指定网卡
    ///
    /// 注意：绑定物理网卡后，流量走该网卡所在网络。若该网络本身不能直连外网
    /// （例如企业网必须经 HTTP 代理），需同时启用 `chain_enabled`，否则连接超时。
    pub egress_interface: String,
    pub connect_timeout_ms: u64,
    pub tcp_nodelay: bool,
    pub tunnel_pool: bool,
    pub auto_reconnect: bool,
    pub auto_switch: bool,

    // ---- 测速 ----
    /// 可选的测速链接列表（界面下拉展示，可自行增删）
    pub speed_urls: Vec<String>,
    /// 当前使用的测速链接
    pub speed_url: String,

    // ---- 日志 ----
    /// 最低输出级别：trace / debug / info / warn / error
    pub log_level: String,
    /// 日志文件路径；留空表示不写文件（仅输出到标准输出与图形界面）
    pub log_file: String,

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

    // ---- Web 控制台 ----
    pub web: WebConfig,
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

            egress_interface: String::new(),
            connect_timeout_ms: 10_000,
            tcp_nodelay: true,
            tunnel_pool: true,
            auto_reconnect: true,
            auto_switch: false,

            speed_urls: default_speed_urls(),
            speed_url: DEFAULT_SPEED_URL.to_string(),

            log_level: "info".to_string(),
            log_file: String::new(),

            autostart: false,
            autostart_connect: true,
            start_minimized: true,
            close_to_tray: true,
            floating: false,
            theme: "system".to_string(),

            current_node: String::new(),
            nodes: Vec::new(),

            web: WebConfig::default(),
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

    /// 实际使用的测速链接；为空时回退到内置默认值。
    pub fn effective_speed_url(&self) -> &str {
        let u = self.speed_url.trim();
        if u.is_empty() {
            DEFAULT_SPEED_URL
        } else {
            u
        }
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
        let mut cfg: Self = toml::from_str(&text).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, format!("config 解析失败: {e}"))
        })?;
        cfg.migrate();
        Ok(cfg)
    }

    /// 修正历史版本写入配置的失效值。
    ///
    /// 旧版把 Cloudflare 测速链接写成了 `bytes=100000000`，而该接口的上限是
    /// 99 MB，请求会被拒绝。这里在读取时自动换成合法值，用户不必手动改配置。
    fn migrate(&mut self) {
        const OLD: &str = "https://speed.cloudflare.com/__down?bytes=100000000";
        const NEW: &str = "https://speed.cloudflare.com/__down?bytes=99000000";

        for u in self.speed_urls.iter_mut() {
            if u == OLD {
                *u = NEW.to_string();
            }
        }
        if self.speed_url == OLD {
            self.speed_url = NEW.to_string();
        }
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
