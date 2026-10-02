//! CloudNProxy 无 GUI 版本。
//!
//! 纯 Rust 实现，不链接 WebView 与图形库，因此可以直接跑在服务器、
//! 容器或最小化 Linux 发行版上。命令行参数只控制「怎么跑」，
//! 其余全部可调项都在 `config.toml` 里。
//!
//! ```text
//! t5d -f /etc/cloudnproxy/config.toml -log debug
//! ```
//!
//! - `-f/--file <path>`  指定配置文件；省略时按平台标准目录查找
//! - `-log/--log <level>` 标准输出日志级别，覆盖 config.toml 的 log_level
//! - 日志文件由 config.toml 的 `log_file` 控制（留空则不落盘）
//! - Web 控制台由 config.toml 的 `[web]` 段控制（默认关闭）

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;
use t5_core::events::Event;
use t5_core::logbuf::{iso_time, level_name, LogLine, LogSink};
use t5_core::{config::Config, Controller, EventBus, Stats};
use tokio::sync::broadcast::error::RecvError;

/// 与 Tauri 版共用同一配置目录，避免两个版本各存一份。
const APP_DIR: &str = "dev.cloudnproxy.app";

const HELP: &str = "\
CloudNProxy daemon — 把 T5 云代理节点转换为本地 SOCKS5 代理（无 GUI 版本）

用法:
  t5d [选项]

选项:
  -f, --file <路径>      配置文件路径（默认按平台标准目录查找）
  -log, --log <级别>     标准输出日志级别: trace|debug|info|warn|error
  -h, --help             显示本帮助
  -v, --version          显示版本

说明:
  其余全部可调项都在 config.toml 中，包括监听地址端口、上游节点、
  FakeHost / X-T5-Auth、Chain 一级代理、隧道池、断线重连、自动切换，
  以及 log_level 与 log_file（log_file 留空表示日志不写入文件）。

Web 控制台（可在浏览器里操作本程序）默认关闭，在 config.toml 中启用：

  [web]
  enabled = true
  listen = \"0.0.0.0:10110\"

  首次启动会生成随机初始密码并打印在日志中，首次登录后必须修改。
  监听在非回环地址时请务必配合防火墙，并优先通过 HTTPS 反向代理访问。
";

#[derive(Default)]
struct Args {
    file: Option<PathBuf>,
    log: Option<String>,
    help: bool,
    version: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);

    while let Some(a) = it.next() {
        match a.as_str() {
            "-f" | "--file" | "-c" | "--config" => {
                args.file = Some(PathBuf::from(it.next().ok_or("缺少配置文件路径")?));
            }
            "-log" | "--log" => {
                args.log = Some(it.next().ok_or("缺少日志级别")?);
            }
            "-h" | "--help" => args.help = true,
            "-v" | "--version" => args.version = true,
            other => {
                if let Some(v) = other.strip_prefix("--file=") {
                    args.file = Some(PathBuf::from(v));
                } else if let Some(v) = other.strip_prefix("--log=") {
                    args.log = Some(v.to_string());
                } else if let Some(v) = other.strip_prefix("-f") {
                    if v.is_empty() {
                        return Err("-f 需要配置文件路径".into());
                    }
                    args.file = Some(PathBuf::from(v));
                } else {
                    return Err(format!("未知参数: {other}"));
                }
            }
        }
    }

    Ok(args)
}

fn default_config_path() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            if !appdata.is_empty() {
                return PathBuf::from(appdata).join(APP_DIR).join("config.toml");
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            if !xdg.is_empty() {
                return PathBuf::from(xdg).join(APP_DIR).join("config.toml");
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            if !home.is_empty() {
                return PathBuf::from(home).join(".config").join(APP_DIR).join("config.toml");
            }
        }
    }

    PathBuf::from("config.toml")
}

fn human(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    let b = bytes as f64;
    if b >= MB {
        format!("{:.2} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// 把一条日志写到标准输出。
fn print_line(line: &LogLine) {
    let mut out = std::io::stdout();
    let _ = writeln!(
        out,
        "{} {:<5} {}",
        iso_time(line.ts),
        line.level,
        line.msg
    );
    let _ = out.flush();
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("参数错误：{e}\n\n{HELP}");
            return ExitCode::from(2);
        }
    };

    if args.help {
        println!("{HELP}");
        return ExitCode::SUCCESS;
    }
    if args.version {
        println!("t5d {}", t5_core::VERSION);
        return ExitCode::SUCCESS;
    }

    let cfg_path = args.file.clone().unwrap_or_else(default_config_path);

    // ---------- 日志 ----------
    let logs = LogSink::new(2000);
    // 事件总线建立之前（读取配置阶段）的日志直接写标准输出
    logs.set_emitter(|line: LogLine| print_line(&line));

    let level = args
        .log
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "info".to_string());
    logs.set_min_level(&level);

    // ---------- 配置 ----------
    let cfg = if cfg_path.exists() {
        match Config::load(&cfg_path) {
            Ok(c) => c,
            Err(e) => {
                logs.error(format!("读取配置失败 {}: {e}", cfg_path.display()));
                return ExitCode::from(1);
            }
        }
    } else {
        let c = Config::default();
        match c.save(&cfg_path) {
            Ok(_) => logs.info(format!("已生成默认配置文件 {}", cfg_path.display())),
            Err(e) => logs.error(format!("写入默认配置失败: {e}")),
        }
        c
    };

    // 命令行未指定 -log 时使用配置里的级别
    if args.log.is_none() {
        logs.set_min_level(&cfg.log_level);
    }
    let effective = level_name(logs.min_level());

    // 日志文件（由配置决定）
    if !cfg.log_file.trim().is_empty() {
        match logs.set_file(Some(PathBuf::from(cfg.log_file.trim()))) {
            Ok(_) => logs.info(format!("日志同时写入 {}", cfg.log_file.trim())),
            Err(e) => logs.error(format!("无法打开日志文件 {}: {e}", cfg.log_file)),
        }
    }

    // ---------- 控制层 ----------
    // 构造 Controller 时会接管日志的 emitter（改为发布到事件总线），
    // 因此下面必须订阅总线才能继续输出标准输出，不能重复设置 emitter。
    let stats = Stats::new();
    let events = EventBus::new();
    let ctrl = Controller::new(
        cfg.clone(),
        logs.clone(),
        stats.clone(),
        events.clone(),
        cfg_path.clone(),
    );

    let mut bg_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();

    // 每秒一次的统计推送。Web 控制台的实时速率图依赖它，没有订阅者时静默。
    bg_tasks.push(tokio::spawn(ctrl.stats_ticker()));

    // 日志：事件总线 → 标准输出
    {
        let mut rx = events.subscribe();
        bg_tasks.push(tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(Event::Log(line)) => print_line(&line),
                    Ok(_) => {}
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                }
            }
        }));
    }

    logs.info(format!("CloudNProxy daemon {} 启动", t5_core::VERSION));
    logs.info(format!("配置文件: {}", cfg_path.display()));
    logs.info(format!("日志级别: {effective}"));

    // ---------- 启动引擎 ----------
    // 先让监听端口就绪，再做网络相关的准备工作：节点解析受上游 DNS 影响，
    // 若放在启动路径上会拖慢服务可用时间（甚至让健康检查误判为启动失败）。
    if let Err(e) = ctrl.start_engine().await {
        logs.error(format!("引擎启动失败: {e}"));
        return ExitCode::from(1);
    }

    // ---------- 出站路径提示 ----------
    // 本机若开着 TUN 模式的代理软件，到节点的流量可能被接管，导致测速失真。
    let target_ip = cfg
        .upstream_addr()
        .split(':')
        .next()
        .unwrap_or("")
        .to_string();
    if !target_ip.is_empty() {
        let info = tokio::task::spawn_blocking(move || t5_core::netinfo::probe(&target_ip))
            .await
            .unwrap_or_default();
        if info.via_tun {
            logs.warn(info.describe());
            logs.warn("提示：可在代理软件的路由规则中把本程序或节点 IP 设为 direct 以绕过 TUN");
        } else {
            logs.info(info.describe());
        }
    }

    // ---------- Web 控制台 ----------
    // 失败只记录错误，不影响 SOCKS5 转发本身
    if cfg.web.enabled {
        match t5_web::start(ctrl.clone()).await {
            Ok(_handle) => {}
            Err(e) => logs.error(format!("Web 控制台启动失败：{e}")),
        }
    } else {
        logs.info("Web 控制台未启用（config.toml 中 [web] enabled = true 可开启）");
    }

    // ---------- 后台刷新节点列表 ----------
    if cfg.nodes.is_empty() && !cfg.resolve_domain.trim().is_empty() {
        let ctrl = ctrl.clone();
        bg_tasks.push(tokio::spawn(async move {
            if let Err(e) = ctrl.resolve_nodes(None).await {
                ctrl.logs.warn(format!("后台解析未完成：{e}"));
            }
        }));
    }

    // 每分钟输出一次统计，便于观察
    {
        let stats = stats.clone();
        let logs = logs.clone();
        bg_tasks.push(tokio::spawn(async move {
            let mut last_up = 0u64;
            let mut last_down = 0u64;
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let s = stats.snapshot();
                let up = s.up_bytes.saturating_sub(last_up);
                let down = s.down_bytes.saturating_sub(last_down);
                last_up = s.up_bytes;
                last_down = s.down_bytes;
                logs.info(format!(
                    "统计: 活动连接 {} · 上行 {}/min · 下行 {}/min · 累计 {}",
                    s.conns,
                    human(up),
                    human(down),
                    human(s.up_bytes + s.down_bytes)
                ));
            }
        }));
    }

    logs.info("SOCKS5 已就绪，按 Ctrl+C 停止");

    // ---------- 等待退出信号 ----------
    #[cfg(unix)]
    {
        let mut term = match tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::terminate(),
        ) {
            Ok(s) => s,
            Err(e) => {
                logs.warn(format!("无法注册 SIGTERM: {e}"));
                let _ = tokio::signal::ctrl_c().await;
                logs.info("收到退出信号，正在停止 …");
                ctrl.stop_engine().await;
                return ExitCode::SUCCESS;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = term.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }

    logs.info("收到退出信号，正在停止 …");

    // 引擎先停，给它一个有限的窗口；随后中止后台任务并直接结束进程。
    //
    // 直接 exit 是刻意的：后台解析里的阻塞式 DNS 查询一旦开始就无法取消，
    // 运行时析构会一直等它返回，导致收到 SIGTERM 后进程迟迟不退出 ——
    // 在 systemd 下会被判定为 stop 超时，最终收到 SIGKILL。
    let _ = tokio::time::timeout(Duration::from_secs(3), ctrl.stop_engine()).await;
    for task in bg_tasks {
        task.abort();
    }
    logs.info("已停止");
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_human_sizes() {
        assert_eq!(human(512), "512 B");
        assert_eq!(human(2048), "2.0 KB");
        assert_eq!(human(5 * 1024 * 1024), "5.00 MB");
    }

    #[test]
    fn level_names_round_trip() {
        for name in ["trace", "debug", "info", "warn", "error"] {
            let code = t5_core::logbuf::level_code(name);
            assert_eq!(level_name(code).to_lowercase(), name);
        }
    }
}
