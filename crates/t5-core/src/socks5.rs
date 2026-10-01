//! SOCKS5 入站：握手、请求解析与应答。
//!
//! 只支持 `CONNECT`（cmd=0x01），与原始 PowerShell 实现保持一致；
//! `UDP ASSOCIATE` 与 `BIND` 分别返回 rep=0x07（命令不支持）。
//! 认证方式无条件选择「无需认证」（0x00）。
//!
//! 状态迁移：
//! ```text
//! Greeting ──► Request ──► (出站建立) ──► Established
//!    │            │
//!    └─ 非 5 版本  └─ cmd≠CONNECT → rep=7
//!                   atyp 非法    → rep=8
//! ```

use std::io;
use std::net::IpAddr;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// SOCKS5 应答码
pub const REP_OK: u8 = 0x00;
pub const REP_GENERAL_FAILURE: u8 = 0x01;
pub const REP_CONN_REFUSED: u8 = 0x05;
pub const REP_CMD_NOT_SUPPORTED: u8 = 0x07;
pub const REP_ATYP_NOT_SUPPORTED: u8 = 0x08;

/// 解析出的目标地址。
#[derive(Debug, Clone)]
pub enum Target {
    Ip(IpAddr, u16),
    Domain(String, u16),
}

impl Target {
    pub fn host(&self) -> String {
        match self {
            Target::Ip(ip, _) => ip.to_string(),
            Target::Domain(d, _) => d.clone(),
        }
    }

    pub fn port(&self) -> u16 {
        match self {
            Target::Ip(_, p) | Target::Domain(_, p) => *p,
        }
    }

    /// `host:port` 形式；IPv6 加方括号。
    pub fn authority(&self) -> String {
        let host = self.host();
        if host.contains(':') {
            format!("[{host}]:{}", self.port())
        } else {
            format!("{host}:{}", self.port())
        }
    }
}

/// 完成 SOCKS5 握手并返回目标地址。
///
/// 成功返回目标地址，此时**尚未**回成功应答 —— 需等出站隧道打通后再调用
/// [`reply`] 回 `REP_OK`。
pub async fn handshake<S>(s: &mut S) -> io::Result<Target>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    // ---- 1. 版本与方法协商 ----
    let ver = s.read_u8().await?;
    if ver != 0x05 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("不是 SOCKS5 协议（ver=0x{ver:02x}）"),
        ));
    }
    let nmethods = s.read_u8().await?;
    let mut methods = vec![0u8; nmethods as usize];
    s.read_exact(&mut methods).await?;
    // 无条件选择「无需认证」
    s.write_all(&[0x05, 0x00]).await?;
    s.flush().await?;

    // ---- 2. 请求 ----
    let ver = s.read_u8().await?;
    if ver != 0x05 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "请求版本错误"));
    }
    let cmd = s.read_u8().await?;
    let _rsv = s.read_u8().await?;
    let atyp = s.read_u8().await?;

    if cmd != 0x01 {
        let _ = reply(s, REP_CMD_NOT_SUPPORTED).await;
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("仅支持 CONNECT，收到 cmd=0x{cmd:02x}"),
        ));
    }

    let (ip_or_domain, is_domain) = match atyp {
        0x01 => {
            let mut a = [0u8; 4];
            s.read_exact(&mut a).await?;
            (IpAddr::from(a).to_string(), false)
        }
        0x03 => {
            let len = s.read_u8().await? as usize;
            if len == 0 {
                let _ = reply(s, REP_ATYP_NOT_SUPPORTED).await;
                return Err(io::Error::new(io::ErrorKind::InvalidData, "域名为空"));
            }
            let mut d = vec![0u8; len];
            s.read_exact(&mut d).await?;
            (String::from_utf8_lossy(&d).to_string(), true)
        }
        0x04 => {
            let mut a = [0u8; 16];
            s.read_exact(&mut a).await?;
            (IpAddr::from(a).to_string(), false)
        }
        _ => {
            let _ = reply(s, REP_ATYP_NOT_SUPPORTED).await;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("不支持的地址类型 atyp=0x{atyp:02x}"),
            ));
        }
    };

    let port = s.read_u16().await?;

    if is_domain {
        Ok(Target::Domain(ip_or_domain, port))
    } else {
        match ip_or_domain.parse::<IpAddr>() {
            Ok(ip) => Ok(Target::Ip(ip, port)),
            Err(_) => Ok(Target::Domain(ip_or_domain, port)),
        }
    }
}

/// 回写 SOCKS5 应答。
pub async fn reply<S>(s: &mut S, rep: u8) -> io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    // VER REP RSV ATYP(IPv4) BND.ADDR(4) BND.PORT(2)
    s.write_all(&[0x05, rep, 0x00, 0x01, 0, 0, 0, 0, 0, 0]).await?;
    s.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_formats_ipv6_with_brackets() {
        let t = Target::Ip("::1".parse().unwrap(), 443);
        assert_eq!(t.authority(), "[::1]:443");
    }

    #[test]
    fn authority_formats_domain() {
        let t = Target::Domain("example.com".into(), 80);
        assert_eq!(t.authority(), "example.com:80");
    }
}
