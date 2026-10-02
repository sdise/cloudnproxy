//! 网卡枚举与出站绑定。
//!
//! # 背景
//!
//! TUN 模式的代理软件（v2rayN / Clash 等）会改写系统路由表把全部出站流量
//! 导入虚拟网卡，绕过清单通常只包含它自己，不含本程序。要让流量改从物理网卡
//! 出去，必须**在 socket 上强制指定出接口**：
//!
//! | 平台 | 机制 | 权限 |
//! |---|---|---|
//! | Linux | `SO_BINDTODEVICE`（按设备名） | 需 root / `CAP_NET_RAW` |
//! | Windows | `IP_UNICAST_IF`（按接口索引，网络字节序） | 一般无需管理员 |
//!
//! 只用 `bind()` 绑定源 IP 是**没用的**：路由查找仍按目标地址进行，包依旧
//! 送往 TUN，只是源地址与出接口不匹配，结果是被丢弃（实测为超时）。
//!
//! # 重要前提
//!
//! 绕过 TUN 之后，流量就走物理网卡所在网络。若那个网络本身不能直连外网
//! （例如企业网必须经 HTTP 代理），就必须同时启用 `chain_enabled`，
//! 否则所有连接都会超时。

use serde::Serialize;
use std::sync::OnceLock;

/// 网卡名或描述里出现这些片段就认为是 TUN / VPN 类。
const TUN_HINTS: &[&str] = &[
    "tun",
    "tap",
    "wintun",
    "utun",
    "clash",
    "mihomo",
    "sing-box",
    "singbox",
    "v2ray",
    "xray",
    "wireguard",
    "tailscale",
    "zerotier",
    "openvpn",
    "nekoray",
    "proxy",
    "vpn",
];

/// 一张网卡。
///
/// 判据来自**接口类型**而非名字猜测：
/// - Windows：`NdisMedium == 19`（`NdisMediumIP`，纯 IP 层）即 TUN；
///   `Virtual = True` 且 `PhysicalMediaType = Unspecified` 即虚拟网卡。
/// - Linux：`/sys/class/net/<if>/type == 65534`（`ARPHRD_NONE`）即 TUN；
///   存在 `/sys/devices/virtual/net/<if>` 即虚拟网卡。
///
/// 名称/描述关键词只作兜底，因为名字可以被用户改，而类型不会。
#[derive(Debug, Clone, Serialize, Default)]
pub struct InterfaceInfo {
    pub name: String,
    /// 接口索引（Windows 绑定用）
    pub index: u32,
    /// 首个 IPv4 地址，可能为空
    pub ipv4: String,
    /// 描述（Windows 上是驱动名，如 `Wintun Tunnel`）
    pub description: String,
    /// 介质类型描述，Windows 上如 `IP` / `802.3` / `Native 802.11`
    pub media: String,
    pub is_up: bool,
    /// 隧道类网卡（TUN/TAP/VPN）
    pub is_tun: bool,
    /// 虚拟网卡（非物理硬件）
    pub is_virtual: bool,
    pub is_loopback: bool,
}

impl InterfaceInfo {
    /// 是否是可用的物理网卡：已启用、非隧道、非虚拟、非回环、且有 IPv4。
    pub fn is_physical_candidate(&self) -> bool {
        self.is_up && !self.is_tun && !self.is_virtual && !self.is_loopback && !self.ipv4.is_empty()
    }

    pub fn kind_label(&self) -> &'static str {
        if self.is_tun {
            "TUN"
        } else if self.is_virtual {
            "虚拟"
        } else {
            "物理"
        }
    }

    pub fn label(&self) -> String {
        let kind = self.kind_label();
        let addr = if self.ipv4.is_empty() {
            String::new()
        } else {
            format!("，{}", self.ipv4)
        };
        let state = if self.is_up { "" } else { "，未启用" };
        format!("{}（{}{}{}）", self.name, kind, addr, state)
    }
}

/// 到某个目标的出站路径。
#[derive(Debug, Clone, Serialize, Default)]
pub struct EgressInfo {
    /// 被探测的目标 IP
    pub target: String,
    /// 实际出接口名称；探测失败为空
    pub interface: String,
    /// 是否疑似经过 TUN / VPN 类网卡
    pub via_tun: bool,
}

impl EgressInfo {
    pub fn describe(&self) -> String {
        if self.interface.is_empty() {
            format!("出站接口未知（目标 {}）", self.target)
        } else if self.via_tun {
            format!(
                "出站经 TUN/VPN（{}），到 {} 的流量被代理软件接管",
                self.interface, self.target
            )
        } else {
            format!("出站直连（{}）→ {}", self.interface, self.target)
        }
    }
}

/// 探测到 `ip` 的出接口。
pub fn probe(ip: &str) -> EgressInfo {
    let mut info = EgressInfo {
        target: ip.to_string(),
        ..Default::default()
    };
    let ip = ip.trim();
    if ip.is_empty() {
        return info;
    }
    if let Some(name) = route_interface(ip) {
        info.via_tun = looks_like_tun(&name);
        info.interface = name;
    }
    info
}

pub fn looks_like_tun(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    TUN_HINTS.iter().any(|h| n.contains(h))
}

/// 进程内缓存的网卡列表。
///
/// 枚举需要调用系统命令，开销在几百毫秒量级，而每次建连都要用到，
/// 因此只在首次调用时枚举一次（插拔网卡后重启程序即可刷新）。
pub fn interfaces() -> &'static Vec<InterfaceInfo> {
    static CACHE: OnceLock<Vec<InterfaceInfo>> = OnceLock::new();
    CACHE.get_or_init(list_interfaces)
}

/// 强制重新枚举（供界面「刷新」使用）。
pub fn list_interfaces() -> Vec<InterfaceInfo> {
    #[cfg(target_os = "windows")]
    {
        list_windows()
    }
    #[cfg(target_os = "linux")]
    {
        list_linux()
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        Vec::new()
    }
}

/// 把配置里的 `egress_interface` 解析成具体的绑定目标。
///
/// - 空字符串：自动挑选物理网卡（推荐用法，等价于「默认走物理网卡」）
/// - `system`：不做绑定，完全跟随系统路由表
/// - 其他：按名称匹配网卡
pub fn binding_for(spec: &str) -> Option<(String, u32)> {
    let spec = spec.trim();

    if spec.eq_ignore_ascii_case("system") {
        return None;
    }

    let list = interfaces();

    if spec.is_empty() {
        return list
            .iter()
            .find(|i| i.is_physical_candidate() && !i.ipv4.is_empty())
            .or_else(|| list.iter().find(|i| i.is_physical_candidate()))
            .map(|i| (i.name.clone(), i.index));
    }

    list.iter()
        .find(|i| i.name.eq_ignore_ascii_case(spec))
        .map(|i| (i.name.clone(), i.index))
}

/// 把出站绑定应用到尚未连接的 socket 上。
///
/// `spec` 为 `system` 或找不到网卡时不做任何事（退回系统路由）。
pub fn apply_binding<T>(sock: &T, spec: &str) -> std::io::Result<Option<String>>
where
    T: BindingTarget,
{
    let Some((name, index)) = binding_for(spec) else {
        return Ok(None);
    };

    #[cfg(target_os = "linux")]
    {
        sock.bind_device(&name)?;
        Ok(Some(name))
    }
    #[cfg(target_os = "windows")]
    {
        sock.bind_interface(index)?;
        Ok(Some(name))
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = (sock, name, index);
        Ok(None)
    }
}

/// 抽象出「可被绑定」的 socket，便于测试与跨平台实现。
pub trait BindingTarget {
    fn bind_device(&self, name: &str) -> std::io::Result<()>;
    fn bind_interface(&self, index: u32) -> std::io::Result<()>;
}

impl BindingTarget for tokio::net::TcpSocket {
    #[cfg(target_os = "linux")]
    fn bind_device(&self, name: &str) -> std::io::Result<()> {
        use std::os::unix::io::AsRawFd;
        setsockopt_bind_device(self.as_raw_fd(), name)
    }

    #[cfg(not(target_os = "linux"))]
    fn bind_device(&self, _name: &str) -> std::io::Result<()> {
        Ok(())
    }

    #[cfg(target_os = "windows")]
    fn bind_interface(&self, index: u32) -> std::io::Result<()> {
        use std::os::windows::io::AsRawSocket;
        setsockopt_unicast_if(self.as_raw_socket(), index)
    }

    #[cfg(not(target_os = "windows"))]
    fn bind_interface(&self, _index: u32) -> std::io::Result<()> {
        Ok(())
    }
}

// ---------------- Linux ----------------

#[cfg(target_os = "linux")]
fn setsockopt_bind_device(fd: std::os::unix::io::RawFd, name: &str) -> std::io::Result<()> {
    extern "C" {
        fn setsockopt(
            fd: i32,
            level: i32,
            optname: i32,
            optval: *const std::ffi::c_void,
            optlen: u32,
        ) -> i32;
    }
    const SOL_SOCKET: i32 = 1;
    const SO_BINDTODEVICE: i32 = 25;

    let cname = std::ffi::CString::new(name)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "网卡名含非法字符"))?;
    let rc = unsafe {
        setsockopt(
            fd,
            SOL_SOCKET,
            SO_BINDTODEVICE,
            cname.as_ptr() as *const std::ffi::c_void,
            cname.as_bytes().len() as u32,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn route_interface(ip: &str) -> Option<String> {
    let out = std::process::Command::new("ip")
        .args(["route", "get", ip])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut words = text.split_whitespace();
    while let Some(w) = words.next() {
        if w == "dev" {
            return words.next().map(|s| s.to_string());
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn list_linux() -> Vec<InterfaceInfo> {
    // ip -br -4 addr show
    // lo               UNKNOWN        127.0.0.1/8
    // eth0             UP             192.168.1.5/24
    let out = match std::process::Command::new("ip")
        .args(["-br", "-4", "addr", "show"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };

    let mut list = Vec::new();
    for (idx, line) in String::from_utf8_lossy(&out.stdout).lines().enumerate() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 3 {
            continue;
        }
        let name = cols[0].to_string();
        let state = cols[1];
        let ipv4 = cols[2].split('/').next().unwrap_or("").to_string();
        let is_loopback = name == "lo";

        // type 来自 sysfs：1 = ARPHRD_ETHER，772 = ARPHRD_LOOPBACK，
        // 65534 = ARPHRD_NONE —— 这正是 TUN（三层）设备的类型。
        let arphrd = std::fs::read_to_string(format!("/sys/class/net/{name}/type"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok());

        // 虚拟网卡都挂在 /sys/devices/virtual/net 下，物理网卡在 PCI/USB 路径下
        let is_virtual = std::path::Path::new("/sys/devices/virtual/net")
            .join(&name)
            .exists();

        let is_tun = arphrd == Some(65534) || looks_like_tun(&name);

        list.push(InterfaceInfo {
            is_tun,
            is_virtual,
            is_up: state.eq_ignore_ascii_case("UP") || state.eq_ignore_ascii_case("UNKNOWN"),
            is_loopback,
            index: idx as u32 + 1,
            media: arphrd.map(|t| t.to_string()).unwrap_or_default(),
            name,
            ipv4,
            description: String::new(),
        });
    }
    list
}

// ---------------- Windows ----------------

#[cfg(target_os = "windows")]
fn setsockopt_unicast_if(
    socket: std::os::windows::io::RawSocket,
    index: u32,
) -> std::io::Result<()> {
    extern "system" {
        fn setsockopt(
            s: usize,
            level: i32,
            optname: i32,
            optval: *const i8,
            optlen: i32,
        ) -> i32;
    }
    const IPPROTO_IP: i32 = 0;
    const IP_UNICAST_IF: i32 = 31;

    // 该选项的值必须是网络字节序的接口索引
    let value = index.to_be();
    let rc = unsafe {
        setsockopt(
            socket as usize,
            IPPROTO_IP,
            IP_UNICAST_IF,
            &value as *const u32 as *const i8,
            4,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn route_interface(ip: &str) -> Option<String> {
    let script = format!(
        "(Find-NetRoute -RemoteIPAddress {ip} -ErrorAction SilentlyContinue | \
         Select-Object -First 1).InterfaceAlias"
    );
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .ok()?;
    let name = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

#[cfg(target_os = "windows")]
fn list_windows() -> Vec<InterfaceInfo> {
    // 除名字外还要取接口类型：Virtual / NdisMedium / MediaType / PhysicalMediaType。
    // 其中 NdisMedium == 19（NdisMediumIP）与 MediaType == "IP" 是 TUN 的确切特征，
    // Virtual == True 则是虚拟网卡的标志 —— 都不依赖网卡名字。
    let script = "Get-NetAdapter | ForEach-Object { $a=$_; \
$ip=(Get-NetIPAddress -InterfaceIndex $a.ifIndex -AddressFamily IPv4 -ErrorAction SilentlyContinue | Select-Object -First 1).IPAddress; \
\"$($a.Name)`t$($a.ifIndex)`t$($a.Status)`t$($a.InterfaceDescription)`t$ip`t$($a.Virtual)`t$($a.NdisMedium)`t$($a.MediaType)`t$($a.PhysicalMediaType)\" }";

    let out = match std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };

    let mut list = Vec::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let cols: Vec<&str> = line.trim_end().split('\t').collect();
        if cols.len() < 4 {
            continue;
        }
        let name = cols[0].trim().to_string();
        if name.is_empty() {
            continue;
        }

        let get = |i: usize| -> String {
            cols.get(i).map(|s| s.trim().to_string()).unwrap_or_default()
        };

        let index = get(1).parse::<u32>().unwrap_or(0);
        let is_up = cols[2].trim().eq_ignore_ascii_case("Up");
        let description = get(3);
        let ipv4 = get(4);

        let virtual_flag = get(5).eq_ignore_ascii_case("True");
        let ndis_medium = get(6).parse::<i32>().unwrap_or(-1);
        let media_type = get(7);
        let physical_media = get(8);

        let is_loopback = name.to_ascii_lowercase().contains("loopback")
            || description.to_ascii_lowercase().contains("loopback");

        // NdisMediumIP(19) / MediaType=IP：纯 IP 层，无以太网封装 —— 典型 TUN
        let is_tun = ndis_medium == 19
            || media_type.eq_ignore_ascii_case("IP")
            || looks_like_tun(&name)
            || looks_like_tun(&description);

        // 适配器自报虚拟，或介质类型不是具体物理介质
        let is_virtual = virtual_flag
            || (!physical_media.is_empty() && physical_media.eq_ignore_ascii_case("Unspecified"));

        list.push(InterfaceInfo {
            is_tun,
            is_virtual,
            is_up,
            is_loopback,
            name,
            index,
            ipv4,
            media: media_type,
            description,
        });
    }
    list
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn route_interface(_ip: &str) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_tun_like_names() {
        assert!(looks_like_tun("Wintun"));
        assert!(looks_like_tun("Clash"));
        assert!(looks_like_tun("Mihomo"));
        assert!(looks_like_tun("utun4"));
        assert!(looks_like_tun("v2rayN"));
        assert!(looks_like_tun("tun0"));
        assert!(looks_like_tun("xray_tun"));
        assert!(looks_like_tun("Wintun Tunnel"));
    }

    #[test]
    fn accepts_physical_names() {
        assert!(!looks_like_tun("Ethernet"));
        assert!(!looks_like_tun("Wi-Fi"));
        assert!(!looks_like_tun("以太网"));
        assert!(!looks_like_tun("WLAN"));
        assert!(!looks_like_tun("enp3s0"));
    }

    #[test]
    fn empty_probe_is_unknown() {
        let info = probe("   ");
        assert!(info.interface.is_empty());
        assert!(!info.via_tun);
    }

    #[test]
    fn system_spec_never_binds() {
        assert!(binding_for("system").is_none());
        assert!(binding_for("SYSTEM").is_none());
        assert!(binding_for(" system ").is_none());
    }

    fn iface(name: &str, ipv4: &str) -> InterfaceInfo {
        InterfaceInfo {
            name: name.into(),
            ipv4: ipv4.into(),
            is_up: true,
            ..Default::default()
        }
    }

    #[test]
    fn physical_candidate_rules() {
        let good = iface("WLAN", "192.168.41.159");
        assert!(good.is_physical_candidate());
        assert_eq!(good.kind_label(), "物理");
        assert_eq!(good.label(), "WLAN（物理，192.168.41.159）");

        // 未启用
        let down = InterfaceInfo {
            is_up: false,
            ..good.clone()
        };
        assert!(!down.is_physical_candidate());

        // 隧道网卡
        let tun = InterfaceInfo {
            is_tun: true,
            ..good.clone()
        };
        assert!(!tun.is_physical_candidate());
        assert_eq!(tun.kind_label(), "TUN");

        // 虚拟网卡（如 TAP 型 VPN 适配器）：不是隧道但也不是物理
        let virt = InterfaceInfo {
            is_virtual: true,
            ..good.clone()
        };
        assert!(!virt.is_physical_candidate());
        assert_eq!(virt.kind_label(), "虚拟");

        // 没有 IPv4 的网卡不能用于绑定
        let no_ip = iface("Ethernet", "");
        assert!(!no_ip.is_physical_candidate());

        // 回环
        let lo = InterfaceInfo {
            is_loopback: true,
            ..good.clone()
        };
        assert!(!lo.is_physical_candidate());
    }
}
