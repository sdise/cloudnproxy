//! 内嵌的前端资源。
//!
//! 与 Tauri 版共用同一份 `ui/` 目录，因此界面只有一套，改动即时对两端生效。
//! 资源在编译期嵌入可执行文件，部署时不需要额外的静态目录。

/// 主页面（含登录 / 改密遮罩层）
pub const INDEX_HTML: &str = include_str!("../../../ui/index.html");
/// 样式表
pub const STYLE_CSS: &str = include_str!("../../../ui/style.css");
/// 前端逻辑（含 Tauri / HTTP 双传输层）
pub const APP_JS: &str = include_str!("../../../ui/app.js");
