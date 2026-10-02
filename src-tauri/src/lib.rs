//! CloudNProxy —— Tauri 应用外壳。
//!
//! 进程模型：UI 与转发引擎同进程。关闭主窗口只是隐藏，引擎继续常驻，
//! 与托盘菜单共同取代了原先的 `start-t5-bg` / `stop-t5-bg` 脚本。
//!
//! 业务逻辑集中在 `t5_core::control::Controller`（与 Web 控制台共用同一份），
//! 这里只负责三件事：注册插件、把事件总线桥接到前端、管理窗口生命周期。

mod commands;
mod tray;

use std::path::PathBuf;
use std::time::Duration;
use tauri::{Emitter, Manager, WindowEvent};
use tauri_plugin_autostart::MacosLauncher;
use t5_core::{Config, Controller, EventBus, LogSink, Stats};
use tokio::sync::broadcast::error::RecvError;

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

            // `-f <path>` 可覆盖默认配置路径，与无 GUI 版本保持一致的用法
            let config_path = arg_config_file().unwrap_or_else(|| {
                handle
                    .path()
                    .app_config_dir()
                    .map(|d| d.join("config.toml"))
                    .unwrap_or_else(|_| PathBuf::from("config.toml"))
            });

            let cfg = Config::load_or_default(&config_path);
            let auto_connect = cfg.autostart_connect;
            let start_minimized = cfg.start_minimized;

            let logs = LogSink::new(2000);
            let stats = Stats::new();
            let events = EventBus::new();

            let ctrl = Controller::new(
                cfg,
                logs.clone(),
                stats.clone(),
                events.clone(),
                config_path.clone(),
            );
            app.manage(ctrl.clone());

            logs.info(format!("CloudNProxy {} 已启动", t5_core::VERSION));
            logs.info(format!("配置文件: {}", config_path.display()));

            // 事件总线 → 前端。
            //
            // 日志、统计、测速进度、状态变化全部从这一处转发，命令层不再各自
            // `emit`，Web 控制台也订阅同一个总线 —— 两种前端因此天然一致。
            {
                let h = handle.clone();
                let mut rx = events.subscribe();
                tauri::async_runtime::spawn(async move {
                    loop {
                        match rx.recv().await {
                            Ok(event) => {
                                let name = event.name();
                                // 前端 `listen(name, e => e.payload)` 拿到的应当是
                                // 事件内容本身，因此这里剥掉 `{"type","payload"}` 外壳。
                                let payload = serde_json::to_value(&event)
                                    .ok()
                                    .and_then(|v| v.get("payload").cloned())
                                    .unwrap_or(serde_json::Value::Null);
                                let _ = h.emit(name, payload);
                            }
                            // 前端短暂卡顿导致的积压丢帧：跳过即可，统计是增量值
                            Err(RecvError::Lagged(_)) => continue,
                            Err(RecvError::Closed) => break,
                        }
                    }
                });
            }

            tray::build(&handle)?;

            // 以 --minimized 启动（自启场景）时隐藏主窗口
            let minimized_launch = std::env::args().any(|a| a == "--minimized");
            if start_minimized && minimized_launch {
                if let Some(w) = handle.get_webview_window("main") {
                    let _ = w.hide();
                }
            }

            // 每秒推送一次统计（无订阅者时自动静默）。
            //
            // 必须经 Tauri 的运行时来 spawn：`setup` 回调跑在主线程的同步上下文，
            // 那里没有 Tokio reactor，直接 `tokio::spawn` 会 panic。
            tauri::async_runtime::spawn(ctrl.stats_ticker());

            // 自启时自动建立代理
            if auto_connect {
                let ctrl = ctrl.clone();
                tauri::async_runtime::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    if let Err(e) = ctrl.start_engine().await {
                        ctrl.logs.error(format!("自动启动代理失败: {e}"));
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
                    .state::<Controller>()
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
            commands::set_speed_url,
            commands::remove_speed_url,
            commands::list_interfaces,
            commands::set_egress_interface,
            commands::is_autostart_enabled,
            commands::set_autostart,
            commands::open_config_dir,
            commands::reset_data,
        ])
        .run(tauri::generate_context!())
        .expect("CloudNProxy 启动失败");
}

/// 解析命令行中的 `-f/--file <path>` / `--file=<path>` / `-f<path>`。
fn arg_config_file() -> Option<PathBuf> {
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-f" | "--file" | "-c" | "--config" => return it.next().map(PathBuf::from),
            _ => {
                if let Some(v) = a.strip_prefix("--file=") {
                    return Some(PathBuf::from(v));
                }
                if let Some(v) = a.strip_prefix("-f") {
                    if !v.is_empty() {
                        return Some(PathBuf::from(v));
                    }
                }
            }
        }
    }
    None
}
