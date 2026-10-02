//! 前端可调用的命令集合。
//!
//! 这里只是**薄封装**：真正的业务逻辑在 `t5_core::control::Controller` 里，
//! 与 Web 控制台共用同一份实现。命名与 `ui/app.js` 中的 `invoke(...)`
//! 一一对应。
//!
//! 实时推送不在命令里做，而是由 `lib.rs` 统一订阅事件总线后转发。

use serde::Serialize;
use std::path::PathBuf;
use tauri::{AppHandle, State};
use tauri_plugin_autostart::ManagerExt;
use t5_core::{bench::BenchResult, Config, Controller, EngineStatus, LogLine, Node, UpdateInfo};

/// `get_status` 的返回体：控制层状态 + 仅供桌面端使用的自启标记。
#[derive(Serialize)]
pub struct StatusPayload {
    #[serde(flatten)]
    pub base: t5_core::StatusPayload,
    pub autostart: bool,
}

#[tauri::command]
pub async fn get_status(
    app: AppHandle,
    state: State<'_, Controller>,
) -> Result<StatusPayload, String> {
    Ok(StatusPayload {
        base: state.status().await,
        autostart: app.autolaunch().is_enabled().unwrap_or(false),
    })
}

#[tauri::command]
pub async fn get_config(state: State<'_, Controller>) -> Result<Config, String> {
    Ok(state.config().await)
}

#[tauri::command]
pub async fn apply_config(
    state: State<'_, Controller>,
    cfg: Config,
) -> Result<Config, String> {
    state.apply_config(cfg).await
}

#[tauri::command]
pub async fn start_proxy(state: State<'_, Controller>) -> Result<EngineStatus, String> {
    state.start().await
}

#[tauri::command]
pub async fn stop_proxy(state: State<'_, Controller>) -> Result<(), String> {
    state.stop().await;
    Ok(())
}

#[tauri::command]
pub async fn get_logs(state: State<'_, Controller>) -> Result<Vec<LogLine>, String> {
    Ok(state.logs_snapshot())
}

#[tauri::command]
pub async fn clear_logs(state: State<'_, Controller>) -> Result<(), String> {
    state.clear_logs();
    Ok(())
}

#[tauri::command]
pub async fn resolve_nodes(
    state: State<'_, Controller>,
    domain: Option<String>,
) -> Result<Vec<Node>, String> {
    state.resolve_nodes(domain).await
}

#[tauri::command]
pub async fn benchmark_all(
    state: State<'_, Controller>,
    only_missing: Option<bool>,
    auto_pick: Option<bool>,
) -> Result<usize, String> {
    state
        .benchmark_all(only_missing.unwrap_or(false), auto_pick.unwrap_or(false))
        .await
}

/// 检查 GitHub 上是否有新版本。
#[tauri::command]
pub async fn check_update(state: State<'_, Controller>) -> Result<UpdateInfo, String> {
    state.check_update().await
}

/// 在系统默认浏览器中打开链接。
#[tauri::command]
pub fn open_url(url: String) -> Result<(), String> {
    let url = url.trim();
    // 限定协议，避免这个入口被当成任意命令执行
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err("仅支持 http/https 链接".into());
    }
    open_external(url)
}

#[tauri::command]
pub async fn benchmark_one(
    state: State<'_, Controller>,
    ip: String,
    port: Option<u16>,
) -> Result<BenchResult, String> {
    Ok(state.benchmark_one(&ip, port.unwrap_or(443)).await)
}

#[tauri::command]
pub async fn set_current_node(
    state: State<'_, Controller>,
    ip: String,
    port: Option<u16>,
) -> Result<(), String> {
    state.set_current_node(&ip, port.unwrap_or(443)).await
}

#[tauri::command]
pub async fn set_speed_url(
    state: State<'_, Controller>,
    url: String,
) -> Result<Vec<String>, String> {
    state.set_speed_url(&url).await
}

#[tauri::command]
pub async fn remove_speed_url(
    state: State<'_, Controller>,
    url: String,
) -> Result<Vec<String>, String> {
    state.remove_speed_url(&url).await
}

#[tauri::command]
pub fn list_interfaces(state: State<'_, Controller>) -> Vec<t5_core::netinfo::InterfaceInfo> {
    state.list_interfaces()
}

#[tauri::command]
pub async fn set_egress_interface(
    state: State<'_, Controller>,
    iface: String,
) -> Result<(), String> {
    state.set_egress_interface(&iface).await
}

#[tauri::command]
pub fn is_autostart_enabled(app: AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}

#[tauri::command]
pub async fn set_autostart(
    state: State<'_, Controller>,
    app: AppHandle,
    enabled: bool,
) -> Result<bool, String> {
    let launcher = app.autolaunch();
    if enabled {
        launcher.enable().map_err(|e| e.to_string())?;
    } else {
        launcher.disable().map_err(|e| e.to_string())?;
    }

    let now = launcher.is_enabled().unwrap_or(false);
    {
        let mut c = state.cfg.lock().await;
        c.autostart = now;
        let _ = c.save(state.config_path.as_path());
    }
    state.logs.info(if now {
        "已开启开机自启"
    } else {
        "已关闭开机自启"
    });
    Ok(now)
}

#[tauri::command]
pub async fn open_config_dir(state: State<'_, Controller>) -> Result<(), String> {
    let dir = state
        .config_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let _ = std::fs::create_dir_all(&dir);
    open_path(&dir).map_err(|e| format!("打开目录失败: {e}"))
}

#[tauri::command]
pub async fn reset_data(state: State<'_, Controller>) -> Result<(), String> {
    state.reset_data().await;
    Ok(())
}

/// 交给系统默认程序打开一个外部目标（链接）。
fn open_external(target: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        // 经 `cmd /C start` 转交给默认浏览器。必须抑制控制台窗口，
        // 否则每次点「GitHub」都会闪一下黑框。
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("cmd")
            .args(["/C", "start", "", target])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| format!("打开链接失败：{e}"))?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(target)
            .spawn()
            .map_err(|e| format!("打开链接失败：{e}"))?;
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = target;
        return Err("当前平台不支持打开链接".into());
    }
    Ok(())
}

fn open_path(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer").arg(path).spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open").arg(path).spawn()?;
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        let _ = path;
    }
    Ok(())
}
