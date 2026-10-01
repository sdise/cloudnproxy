//! CloudNProxy —— Tauri 应用外壳。
//!
//! 进程模型：UI 与转发引擎同进程。关闭主窗口只是隐藏，引擎继续常驻，
//! 与托盘菜单共同取代了原先的 `start-t5-bg` / `stop-t5-bg` 脚本。

mod commands;
mod state;
mod tray;

use serde::Serialize;
use std::time::Duration;
use tauri::{Emitter, Manager, WindowEvent};
use tauri_plugin_autostart::MacosLauncher;
use t5_core::{Config, LogSink, Stats};

/// 每秒推送给前端的实时统计（字节率为「上一秒增量」）。
#[derive(Clone, Serialize)]
struct StatsTick {
    up_rate: u64,
    down_rate: u64,
    up_bytes: u64,
    down_bytes: u64,
    conns: i64,
    sessions: u64,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // 单实例插件必须最先注册
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            tray::show_main(app);
        }))
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec!["--minimized"]),
        ))
        .setup(|app| {
            let handle = app.handle().clone();

            let config_path = handle
                .path()
                .app_config_dir()
                .map(|d| d.join("config.toml"))
                .unwrap_or_else(|_| std::path::PathBuf::from("config.toml"));

            let cfg = Config::load_or_default(&config_path);
            let auto_connect = cfg.autostart_connect;
            let start_minimized = cfg.start_minimized;

            let logs = LogSink::new(2000);
            let stats = Stats::new();

            // 日志实时推送到前端（不落盘）
            {
                let h = handle.clone();
                logs.set_emitter(move |line| {
                    let _ = h.emit("log", &line);
                });
            }

            logs.info(format!("CloudNProxy {} 已启动", t5_core::VERSION));
            logs.info(format!("配置文件: {}", config_path.display()));

            app.manage(state::AppState::new(
                cfg,
                logs.clone(),
                stats.clone(),
                config_path,
            ));

            tray::build(&handle)?;

            // 以 --minimized 启动（自启场景）时隐藏主窗口
            let minimized_launch = std::env::args().any(|a| a == "--minimized");
            if start_minimized && minimized_launch {
                if let Some(w) = handle.get_webview_window("main") {
                    let _ = w.hide();
                }
            }

            // 每秒推送统计
            {
                let h = handle.clone();
                let s = stats.clone();
                tauri::async_runtime::spawn(async move {
                    let mut last_up = 0u64;
                    let mut last_down = 0u64;
                    loop {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        let snap = s.snapshot();
                        let tick = StatsTick {
                            up_rate: snap.up_bytes.saturating_sub(last_up),
                            down_rate: snap.down_bytes.saturating_sub(last_down),
                            up_bytes: snap.up_bytes,
                            down_bytes: snap.down_bytes,
                            conns: snap.conns,
                            sessions: snap.sessions,
                        };
                        last_up = snap.up_bytes;
                        last_down = snap.down_bytes;
                        let _ = h.emit("stats", &tick);
                    }
                });
            }

            // 自启时自动建立代理
            if auto_connect {
                let h = handle.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    let st = h.state::<state::AppState>();
                    if let Err(e) = state::start_engine(st.inner()).await {
                        st.logs.error(format!("自动启动代理失败: {e}"));
                    }
                });
            }

            Ok(())
        })
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                if window.label() != "main" {
                    return;
                }
                let keep = window
                    .app_handle()
                    .state::<state::AppState>()
                    .cfg
                    .try_lock()
                    .map(|c| c.close_to_tray)
                    .unwrap_or(true);
                if keep {
                    api.prevent_close();
                    let _ = window.hide();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_status,
            commands::get_config,
            commands::apply_config,
            commands::start_proxy,
            commands::stop_proxy,
            commands::get_logs,
            commands::clear_logs,
            commands::resolve_nodes,
            commands::benchmark_all,
            commands::benchmark_one,
            commands::set_current_node,
            commands::is_autostart_enabled,
            commands::set_autostart,
            commands::open_config_dir,
            commands::reset_data,
        ])
        .run(tauri::generate_context!())
        .expect("CloudNProxy 启动失败");
}
