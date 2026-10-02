//! t5-web —— CloudNProxy 的 Web 控制台。
//!
//! 让无 GUI 版本（`t5d`）也能在浏览器里被操作：界面直接复用 Tauri 版的
//! `ui/` 资源，后端提供等价的 JSON 命令接口与 SSE 事件流。
//!
//! ```no_run
//! # async fn demo(ctrl: t5_core::Controller) -> Result<(), String> {
//! let handle = t5_web::start(ctrl).await?;   // 未启用时返回 None
//! # let _ = handle;
//! # Ok(())
//! # }
//! ```
//!
//! 安全模型：
//! - 默认**不启用**，需在 `config.toml` 的 `[web]` 段显式打开；
//! - 首次启动生成随机初始密码，首次登录后强制修改；
//! - 登录返回 HS256 JWT，接口全部要求 `Authorization: Bearer`；
//! - 登录失败按来源 IP 指数退避锁定。

pub mod assets;
pub mod auth;
pub mod server;

pub use auth::{Authenticator, Claims, LoginOutcome};
pub use server::{start, WebApp};
