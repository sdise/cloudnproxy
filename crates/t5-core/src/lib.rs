//! t5-core —— 把百度 T5 云代理节点（靠 `Host` + `X-T5-Auth` 鉴权的 HTTP CONNECT
//! 代理）转换成标准本地 SOCKS5 代理的转发引擎。
//!
//! 模块划分：
//! - [`config`]    配置结构与 TOML 读写
//! - [`logbuf`]    内存环形日志缓冲（不落盘）
//! - [`stats`]     上下行字节与连接数统计
//! - [`socks5`]    SOCKS5 入站握手与请求解析
//! - [`outbound`]  T5 出站隧道建立（含可选一级 Chain 代理）
//! - [`engine`]    监听循环与双向转发
//! - [`resolver`]  DoH 域名解析与 GeoIP 查询
//! - [`bench`]     节点延迟 / 带宽测速
//! - [`tunnel_pool`] 上游隧道预建与复用
//! - [`temp_proxy`] 测速用的临时单节点 SOCKS5 出口
//! - [`netinfo`]   出站路径探测（是否经过 TUN）
//! - [`events`]    控制台事件总线（所有实时推送的唯一出处）
//! - [`control`]   与前端无关的控制层，供 Tauri 与 Web 控制台共用

pub mod bench;
pub mod config;
pub mod control;
pub mod engine;
pub mod events;
pub mod logbuf;
pub mod netinfo;
pub mod outbound;
pub mod resolver;
pub mod socks5;
pub mod stats;
pub mod temp_proxy;
pub mod tunnel_pool;

pub use config::{Config, Node, WebConfig};
pub use control::{Controller, StatusPayload, UpdateInfo};
pub use engine::{EngineHandle, EngineStatus};
pub use events::{Event, EventBus};
pub use logbuf::{LogLine, LogSink};
pub use stats::{Stats, StatsSnapshot};

/// crate 版本号，供 UI 显示。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
