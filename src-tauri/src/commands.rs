//! 前端可调用的命令集合。
//!
//! 命名与 `ui/app.js` 中的 `invoke(...)` 一一对应。

use crate::state::{start_engine, stop_engine, AppState};
use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use tauri::{AppHandle, Emitter, State};
use tauri_plugin_autostart::ManagerExt;
use t5_core::{bench::BenchResult, logbuf, Config, EngineStatus, LogLine, Node};

#[derive(Serialize)]
pub struct StatusPayload {
    pub engine: EngineStatus,
    pub stats: t5_core::StatsSnapshot,
    pub nodes: usize,
    pub resolve_domain: String,
    pub autostart: bool,
    pub config_path: String,
    /// 到当前上游节点的出站路径（是否被 TUN 接管）
    pub egress: t5_core::netinfo::EgressInfo,
}

#[tauri::command]
pub async fn get_status(app: AppHandle, state: State<'_, AppState>) -> Result<StatusPayload, String> {
    let cfg = state.cfg.lock().await.clone();
    let engine = {
        let guard = state.engine.lock().await;
        match guard.as_ref() {
            Some(h) => h.status(),
            None => EngineStatus {
                running: false,
                addr: cfg.listen_addr(),
                upstream: cfg.upstream_addr(),
                chain: if cfg.chain_enabled && !cfg.chain_addr.trim().is_empty() {
                    cfg.chain_addr.clone()
                } else {
                    "直连".to_string()
                },
                tunnel_pool: 0,
            },
        }
    };

    // 探测到节点的出站路径。查询要走系统命令，放到阻塞线程池避免拖住异步运行时。
    let target_ip = cfg
        .upstream_addr()
        .split(':')
        .next()
        .unwrap_or("")
        .to_string();
    let egress = if target_ip.is_empty() {
        t5_core::netinfo::EgressInfo::default()
    } else {
        tokio::task::spawn_blocking(move || t5_core::netinfo::probe(&target_ip))
            .await
            .unwrap_or_default()
    };

    Ok(StatusPayload {
        engine,
        stats: state.stats.snapshot(),
        nodes: cfg.nodes.len(),
        resolve_domain: cfg.resolve_domain.clone(),
        autostart: app.autolaunch().is_enabled().unwrap_or(false),
        config_path: state.config_path.display().to_string(),
        egress,
    })
}

#[tauri::command]
pub async fn get_config(state: State<'_, AppState>) -> Result<Config, String> {
    Ok(state.cfg.lock().await.clone())
}

#[tauri::command]
pub async fn apply_config(
    app: AppHandle,
    state: State<'_, AppState>,
    cfg: Config,
) -> Result<Config, String> {
    let was_running = {
        let guard = state.engine.lock().await;
        guard.as_ref().map(|h| h.is_running()).unwrap_or(false)
    };

    if was_running {
        stop_engine(state.inner()).await;
    }

    cfg.save(state.config_path.as_path())
        .map_err(|e| format!("保存配置失败: {e}"))?;
    *state.cfg.lock().await = cfg.clone();

    if was_running {
        start_engine(state.inner()).await?;
    }

    state.logs.info("配置已应用");
    let _ = app.emit("config-changed", &cfg);
    Ok(cfg)
}

#[tauri::command]
pub async fn start_proxy(app: AppHandle, state: State<'_, AppState>) -> Result<EngineStatus, String> {
    start_engine(state.inner()).await?;
    let status = {
        let guard = state.engine.lock().await;
        guard
            .as_ref()
            .map(|h| h.status())
            .unwrap_or_else(|| EngineStatus {
                running: false,
                addr: String::new(),
                upstream: String::new(),
                chain: String::new(),
                tunnel_pool: 0,
            })
    };
    let _ = app.emit("status-changed", &status);
    Ok(status)
}

#[tauri::command]
pub async fn stop_proxy(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    stop_engine(state.inner()).await;
    let _ = app.emit("status-changed", serde_json::json!({ "running": false }));
    Ok(())
}

#[tauri::command]
pub async fn get_logs(state: State<'_, AppState>) -> Result<Vec<LogLine>, String> {
    Ok(state.logs.snapshot())
}

#[tauri::command]
pub async fn clear_logs(state: State<'_, AppState>) -> Result<(), String> {
    state.logs.clear();
    Ok(())
}

/// 解析来源域名，把结果合并进节点库（保留已有节点的测速数据）。
#[tauri::command]
pub async fn resolve_nodes(
    app: AppHandle,
    state: State<'_, AppState>,
    domain: Option<String>,
) -> Result<Vec<Node>, String> {
    let domain = {
        let cfg = state.cfg.lock().await;
        domain
            .unwrap_or_else(|| cfg.resolve_domain.clone())
            .trim()
            .to_string()
    };
    if domain.is_empty() {
        return Err("域名不能为空".into());
    }

    state
        .logs
        .info(format!("解析 {domain} …（多源 DoH + 多地域 ECS）"));
    let report = t5_core::resolver::resolve_domain(&state.client, &domain).await;
    if report.ips.is_empty() {
        state.logs.warn(format!("解析 {domain} 未获得任何 IP"));
        return Err(format!("解析 {domain} 失败，请检查网络"));
    }

    let nodes = {
        let mut cfg = state.cfg.lock().await;
        cfg.resolve_domain = domain.clone();
        cfg.merge_node_ips(&report.ips);
        let _ = cfg.save(state.config_path.as_path());
        cfg.nodes.clone()
    };

    state.logs.info(format!(
        "解析 {domain} → {} 个节点（来源 {}）",
        nodes.len(),
        report.summary()
    ));
    let _ = app.emit("nodes-changed", &nodes);
    Ok(nodes)
}

/// 批量测速。立即返回节点总数，进度通过 `bench-progress` 事件推送。
#[tauri::command]
pub async fn benchmark_all(
    app: AppHandle,
    state: State<'_, AppState>,
    only_missing: Option<bool>,
) -> Result<usize, String> {
    if state.bench_running.swap(true, Ordering::SeqCst) {
        return Err("测速正在进行中".into());
    }

    let cfg_arc = state.cfg.clone();
    let logs = state.logs.clone();
    let client = state.client.clone();
    let path = state.config_path.clone();
    let flag = state.bench_running.clone();
    let handle = app.clone();

    let targets: Vec<Node> = cfg_arc.lock().await.nodes.clone();
    let total = targets.len();
    let only = only_missing.unwrap_or(false);

    tauri::async_runtime::spawn(async move {
        let cfg = cfg_arc.lock().await.clone();
        for (i, node) in targets.iter().enumerate() {
            if only && node.speed_mbps.is_some() {
                continue;
            }
            let addr = node.addr();
            let res = t5_core::bench::benchmark(&cfg, &addr, &client).await;

            {
                let mut c = cfg_arc.lock().await;
                if let Some(n) = c
                    .nodes
                    .iter_mut()
                    .find(|n| n.ip == node.ip && n.port == node.port)
                {
                    n.latency_ms = res.latency_ms;
                    n.speed_mbps = res.speed_mbps;
                    n.region = res.region.clone();
                    n.entry_isp = res.entry_isp.clone();
                    n.exit_ip = res.exit_ip.clone();
                    n.exit_region = res.exit_region.clone();
                    n.exit_isp = res.exit_isp.clone();
                    n.exit_asn = res.exit_asn.clone();
                    n.measured_at = Some(logbuf::now_secs());
                }
                let _ = c.save(path.as_path());
            }

            let _ = handle.emit(
                "bench-progress",
                serde_json::json!({
                    "index": i + 1,
                    "total": total,
                    "ip": node.ip,
                    "result": &res,
                }),
            );

            match (res.speed_mbps, res.latency_ms) {
                (Some(speed), Some(lat)) => logs.info(format!(
                    "{addr} → {speed:.2} Mbps, {lat} ms（下载 {:.2} MB / {:.1} 秒）",
                    res.bytes as f64 / 1_048_576.0,
                    res.seconds.unwrap_or(0.0)
                )),
                _ => logs.warn(format!(
                    "{addr} → 失败：{}",
                    res.error.clone().unwrap_or_else(|| "无数据".into())
                )),
            }
        }

        flag.store(false, Ordering::SeqCst);
        let _ = handle.emit("bench-done", serde_json::json!({ "total": total }));
        logs.info("全部测速完成");
    });

    Ok(total)
}

/// 单个节点测速，并把结果写回配置。
#[tauri::command]
pub async fn benchmark_one(
    app: AppHandle,
    state: State<'_, AppState>,
    ip: String,
    port: Option<u16>,
) -> Result<BenchResult, String> {
    let cfg = state.cfg.lock().await.clone();
    let addr = format!("{}:{}", ip, port.unwrap_or(443));

    let res = t5_core::bench::benchmark(&cfg, &addr, &state.client).await;

    {
        let mut c = state.cfg.lock().await;
        if let Some(n) = c.nodes.iter_mut().find(|n| n.ip == ip) {
            n.latency_ms = res.latency_ms;
            n.speed_mbps = res.speed_mbps;
            n.region = res.region.clone();
            n.entry_isp = res.entry_isp.clone();
            n.exit_ip = res.exit_ip.clone();
            n.exit_region = res.exit_region.clone();
            n.exit_isp = res.exit_isp.clone();
            n.exit_asn = res.exit_asn.clone();
            n.measured_at = Some(logbuf::now_secs());
        }
        let _ = c.save(state.config_path.as_path());
    }

    match res.speed_mbps {
        Some(speed) => state.logs.info(format!(
            "{addr} → {speed:.2} Mbps, {} ms（下载 {:.2} MB / {:.1} 秒）",
            res.latency_ms.unwrap_or(0),
            res.bytes as f64 / 1_048_576.0,
            res.seconds.unwrap_or(0.0)
        )),
        None => state.logs.warn(format!(
            "{addr} → 失败：{}",
            res.error.clone().unwrap_or_else(|| "无数据".into())
        )),
    }

    let _ = app.emit("node-updated", &res);
    Ok(res)
}

/// 切换当前使用的节点。引擎运行中直接热切换，不中断已有连接。
#[tauri::command]
pub async fn set_current_node(
    app: AppHandle,
    state: State<'_, AppState>,
    ip: String,
    port: Option<u16>,
) -> Result<(), String> {
    let addr = format!("{}:{}", ip, port.unwrap_or(443));

    {
        let mut c = state.cfg.lock().await;
        c.current_node = addr.clone();
        c.upstream = addr.clone();
        c.save(state.config_path.as_path())
            .map_err(|e| format!("保存配置失败: {e}"))?;
    }

    let switched = {
        let guard = state.engine.lock().await;
        match guard.as_ref() {
            Some(h) if h.is_running() => {
                h.set_upstream(&addr);
                true
            }
            _ => false,
        }
    };

    if switched {
        state.logs.info(format!("当前节点已热切换为 {addr}"));
    } else {
        state.logs.info(format!("当前节点已设置为 {addr}（未运行）"));
    }
    let _ = app.emit("config-changed", ());
    Ok(())
}

#[tauri::command]
pub fn is_autostart_enabled(app: AppHandle) -> bool {
    app.autolaunch().is_enabled().unwrap_or(false)
}

#[tauri::command]
pub async fn set_autostart(
    state: State<'_, AppState>,
    app: AppHandle,
    enabled: bool,
) -> Result<bool, String> {
    let launcher = app.autolaunch();
    let result = if enabled {
        launcher.enable().map_err(|e| e.to_string())?
    } else {
        launcher.disable().map_err(|e| e.to_string())?
    };
    let _ = result;

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

/// 选定一个测速链接；若不在列表中则一并加入。返回更新后的链接列表。
#[tauri::command]
pub async fn set_speed_url(
    state: State<'_, AppState>,
    url: String,
) -> Result<Vec<String>, String> {
    let url = url.trim().to_string();
    if url.is_empty() {
        return Err("测速链接不能为空".into());
    }
    if !url.starts_with("http://") && !url.starts_with("https://") {
        return Err("测速链接需以 http:// 或 https:// 开头".into());
    }

    let list = {
        let mut c = state.cfg.lock().await;
        if !c.speed_urls.iter().any(|u| u == &url) {
            c.speed_urls.push(url.clone());
        }
        c.speed_url = url.clone();
        c.save(state.config_path.as_path())
            .map_err(|e| format!("保存配置失败: {e}"))?;
        c.speed_urls.clone()
    };

    state.logs.info(format!("测速链接已设为 {url}"));
    Ok(list)
}

/// 删除一个测速链接；若删的正是当前链接，则回退到列表首项。
#[tauri::command]
pub async fn remove_speed_url(
    state: State<'_, AppState>,
    url: String,
) -> Result<Vec<String>, String> {
    let list = {
        let mut c = state.cfg.lock().await;
        c.speed_urls.retain(|u| u != &url);
        if c.speed_urls.is_empty() {
            c.speed_urls
                .push(t5_core::config::DEFAULT_SPEED_URL.to_string());
        }
        if c.speed_url.trim().is_empty() || c.speed_url == url {
            c.speed_url = c.speed_urls[0].clone();
        }
        c.save(state.config_path.as_path())
            .map_err(|e| format!("保存配置失败: {e}"))?;
        c.speed_urls.clone()
    };

    state.logs.info(format!("已删除测速链接 {url}"));
    Ok(list)
}

/// 列出本机网卡，供「出站网卡」下拉使用。
#[tauri::command]
pub fn list_interfaces() -> Vec<t5_core::netinfo::InterfaceInfo> {
    t5_core::netinfo::list_interfaces()
}

/// 设置出站网卡绑定；引擎运行中会重启以生效。
#[tauri::command]
pub async fn set_egress_interface(
    app: AppHandle,
    state: State<'_, AppState>,
    iface: String,
) -> Result<(), String> {
    let was_running = {
        let guard = state.engine.lock().await;
        guard.as_ref().map(|h| h.is_running()).unwrap_or(false)
    };

    let chain_ready = {
        let mut c = state.cfg.lock().await;
        c.egress_interface = iface.clone();
        c.save(state.config_path.as_path())
            .map_err(|e| format!("保存配置失败: {e}"))?;
        c.chain_enabled && !c.chain_addr.trim().is_empty()
    };

    if was_running {
        stop_engine(state.inner()).await;
        start_engine(state.inner()).await?;
    }

    let spec = iface.trim();
    if spec.is_empty() {
        state.logs.info("出站网卡：自动选择物理网卡（绕过 TUN）");
    } else if spec.eq_ignore_ascii_case("system") {
        state.logs.info("出站网卡：跟随系统路由（可能被 TUN 接管）");
    } else {
        state.logs.info(format!("出站网卡已绑定为 {spec}"));
    }

    if !chain_ready && !spec.is_empty() && !spec.eq_ignore_ascii_case("system") {
        state.logs.warn(
            "已绑定物理网卡但未启用 Chain；若该网络需要经代理才能出网，连接会全部超时",
        );
    }

    let _ = app.emit("config-changed", ());
    Ok(())
}

#[tauri::command]
pub async fn open_config_dir(state: State<'_, AppState>) -> Result<(), String> {
    let dir = state
        .config_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let _ = std::fs::create_dir_all(&dir);
    open_path(&dir).map_err(|e| format!("打开目录失败: {e}"))
}

#[tauri::command]
pub async fn reset_data(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    stop_engine(state.inner()).await;
    let _ = std::fs::remove_file(state.config_path.as_path());
    *state.cfg.lock().await = Config::default();
    state.logs.clear();
    state.logs.info("已重置配置与测速结果");
    let _ = app.emit("config-changed", ());
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
