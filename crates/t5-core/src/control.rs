//! 控制层：把配置、引擎、测速、日志、统计聚合成一套**与前端无关**的操作接口。
//!
//! Tauri 命令与 Web 控制台的 HTTP 接口都只是这一层的薄封装，因此两种前端
//! 的行为完全一致，业务逻辑只存在一份。
//!
//! 所有异步结果通过 [`EventBus`] 广播，前端各自订阅：
//! Tauri 转成 `emit`，Web 控制台转成 SSE。

use crate::bench::BenchResult;
use crate::config::{Config, Node};
use crate::engine::{EngineHandle, EngineStatus};
use crate::events::{BenchDone, BenchProgress, BenchStarted, Event, EventBus, StatsTick};
use crate::logbuf::{self, LogLine, LogSink};
use crate::netinfo::{EgressInfo, InterfaceInfo};
use crate::stats::{Stats, StatsSnapshot};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

/// 出站路径探测结果的缓存时长。
///
/// 探测要执行系统命令（Windows 的 PowerShell、Linux 的 `ip route`），
/// 而多个前端可能同时刷新状态，因此做一次短缓存，避免重复拉起子进程。
const EGRESS_CACHE_TTL: Duration = Duration::from_secs(3);

/// 引擎状态 + 运行指标 + 出站路径。`get_status` 的返回体。
#[derive(Debug, Clone, Serialize)]
pub struct StatusPayload {
    pub engine: EngineStatus,
    pub stats: StatsSnapshot,
    pub nodes: usize,
    pub resolve_domain: String,
    pub config_path: String,
    /// 到当前上游节点的出站路径（是否被 TUN 接管）
    pub egress: EgressInfo,
    /// 程序版本，供界面「关于」显示
    pub version: String,
}

type EgressCache = Option<(Instant, String, EgressInfo)>;

/// 应用控制层。内部全部是 `Arc`，可自由 clone 进后台任务。
#[derive(Clone)]
pub struct Controller {
    pub cfg: Arc<Mutex<Config>>,
    pub engine: Arc<Mutex<Option<EngineHandle>>>,
    pub bench_running: Arc<AtomicBool>,
    pub logs: LogSink,
    pub stats: Stats,
    pub events: EventBus,
    pub client: reqwest::Client,
    pub config_path: Arc<PathBuf>,
    egress_cache: Arc<StdMutex<EgressCache>>,
}

impl Controller {
    /// 构造控制层，并把日志接入事件总线。
    ///
    /// 日志的唯一出口就是总线，因此调用方**不要**再调用 `logs.set_emitter`，
    /// 否则会覆盖这里的接线。需要把日志输出到标准输出或界面时，订阅总线即可。
    pub fn new(
        cfg: Config,
        logs: LogSink,
        stats: Stats,
        events: EventBus,
        config_path: PathBuf,
    ) -> Self {
        {
            let events = events.clone();
            logs.set_emitter(move |line: LogLine| events.publish(Event::Log(line)));
        }

        Self {
            cfg: Arc::new(Mutex::new(cfg)),
            engine: Arc::new(Mutex::new(None)),
            bench_running: Arc::new(AtomicBool::new(false)),
            logs,
            stats,
            events,
            client: crate::resolver::http_client(),
            config_path: Arc::new(config_path),
            egress_cache: Arc::new(StdMutex::new(None)),
        }
    }

    // ---------------- 状态 ----------------

    pub async fn status(&self) -> StatusPayload {
        let (cfg, engine) = {
            let cfg = self.cfg.lock().await.clone();
            let engine = self.engine_status(&cfg).await;
            (cfg, engine)
        };

        StatusPayload {
            engine,
            stats: self.stats.snapshot(),
            nodes: cfg.nodes.len(),
            resolve_domain: cfg.resolve_domain.clone(),
            config_path: self.config_path.display().to_string(),
            egress: self.egress(&cfg).await,
            version: crate::VERSION.to_string(),
        }
    }

    /// 列出网卡，供「出站网卡」下拉使用。
    pub fn list_interfaces(&self) -> Vec<InterfaceInfo> {
        crate::netinfo::list_interfaces()
    }

    async fn engine_status(&self, cfg: &Config) -> EngineStatus {
        let guard = self.engine.lock().await;
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
    }

    /// 探测到上游节点的出站路径，带短缓存。
    async fn egress(&self, cfg: &Config) -> EgressInfo {
        let target_ip = cfg
            .upstream_addr()
            .split(':')
            .next()
            .unwrap_or("")
            .to_string();
        if target_ip.is_empty() {
            return EgressInfo::default();
        }

        if let Ok(guard) = self.egress_cache.lock() {
            if let Some((at, ip, info)) = guard.as_ref() {
                if ip == &target_ip && at.elapsed() < EGRESS_CACHE_TTL {
                    return info.clone();
                }
            }
        }

        let probe_ip = target_ip.clone();
        let info = tokio::task::spawn_blocking(move || crate::netinfo::probe(&probe_ip))
            .await
            .unwrap_or_default();

        if let Ok(mut guard) = self.egress_cache.lock() {
            *guard = Some((Instant::now(), target_ip, info.clone()));
        }
        info
    }

    // ---------------- 配置 ----------------

    pub async fn config(&self) -> Config {
        self.cfg.lock().await.clone()
    }

    /// 保存并应用配置；原先在运行则重启引擎使其生效。
    pub async fn apply_config(&self, cfg: Config) -> Result<Config, String> {
        let was_running = self.is_running().await;
        if was_running {
            self.stop_engine().await;
        }

        cfg.save(self.config_path.as_path())
            .map_err(|e| format!("保存配置失败: {e}"))?;
        *self.cfg.lock().await = cfg.clone();

        if was_running {
            self.start_engine().await?;
        }

        self.logs.info("配置已应用");
        self.events.publish(Event::ConfigChanged);
        self.events.publish(Event::StatusChanged(self.status().await.engine));
        Ok(cfg)
    }

    /// 删除配置文件、恢复默认值并清空测速结果。
    pub async fn reset_data(&self) {
        self.stop_engine().await;
        let _ = std::fs::remove_file(self.config_path.as_path());
        *self.cfg.lock().await = Config::default();
        self.logs.clear();
        self.logs.info("已重置配置与测速结果");
        self.events.publish(Event::ConfigChanged);
        self.events.publish(Event::StatusChanged(self.status().await.engine));
    }

    // ---------------- 引擎 ----------------

    pub async fn is_running(&self) -> bool {
        let guard = self.engine.lock().await;
        guard.as_ref().map(|h| h.is_running()).unwrap_or(false)
    }

    pub async fn start_engine(&self) -> Result<(), String> {
        if self.is_running().await {
            return Ok(());
        }
        let cfg = self.cfg.lock().await.clone();
        let handle = crate::engine::start(cfg, self.logs.clone(), self.stats.clone())
            .await
            .map_err(|e| e.to_string())?;
        *self.engine.lock().await = Some(handle);
        Ok(())
    }

    pub async fn stop_engine(&self) {
        let handle = {
            let mut guard = self.engine.lock().await;
            guard.take()
        };
        if let Some(h) = handle {
            h.shutdown().await;
        }
    }

    /// 启动引擎并广播状态变化。
    pub async fn start(&self) -> Result<EngineStatus, String> {
        self.start_engine().await?;
        let status = self.status().await.engine;
        self.events.publish(Event::StatusChanged(status.clone()));
        Ok(status)
    }

    /// 停止引擎并广播状态变化（幂等）。
    pub async fn stop(&self) {
        self.stop_engine().await;
        self.events.publish(Event::StatusChanged(self.status().await.engine));
    }

    // ---------------- 日志 ----------------

    pub fn logs_snapshot(&self) -> Vec<LogLine> {
        self.logs.snapshot()
    }

    pub fn clear_logs(&self) {
        self.logs.clear();
    }

    // ---------------- 节点 ----------------

    /// 解析来源域名并合并进节点库（保留已有节点的测速数据）。
    pub async fn resolve_nodes(&self, domain: Option<String>) -> Result<Vec<Node>, String> {
        let domain = {
            let cfg = self.cfg.lock().await;
            domain
                .unwrap_or_else(|| cfg.resolve_domain.clone())
                .trim()
                .to_string()
        };
        if domain.is_empty() {
            return Err("域名不能为空".into());
        }

        self.logs
            .info(format!("解析 {domain} …（多源 DoH + 多地域 ECS）"));
        let report = crate::resolver::resolve_domain(&self.client, &domain).await;
        if report.ips.is_empty() {
            self.logs.warn(format!("解析 {domain} 未获得任何 IP"));
            return Err(format!("解析 {domain} 失败，请检查网络"));
        }

        let nodes = {
            let mut cfg = self.cfg.lock().await;
            cfg.resolve_domain = domain.clone();
            cfg.merge_node_ips(&report.ips);
            let _ = cfg.save(self.config_path.as_path());
            cfg.nodes.clone()
        };

        self.logs.info(format!(
            "解析 {domain} → {} 个节点（来源 {}）",
            nodes.len(),
            report.summary()
        ));
        self.events.publish(Event::NodesChanged(nodes.clone()));
        Ok(nodes)
    }

    /// 切换当前使用的节点。引擎运行中直接热切换，不中断已有连接。
    pub async fn set_current_node(&self, ip: &str, port: u16) -> Result<(), String> {
        let addr = format!("{ip}:{port}");

        {
            let mut c = self.cfg.lock().await;
            c.current_node = addr.clone();
            c.upstream = addr.clone();
            c.save(self.config_path.as_path())
                .map_err(|e| format!("保存配置失败: {e}"))?;
        }

        let switched = {
            let guard = self.engine.lock().await;
            match guard.as_ref() {
                Some(h) if h.is_running() => {
                    h.set_upstream(&addr);
                    true
                }
                _ => false,
            }
        };

        if switched {
            self.logs.info(format!("当前节点已热切换为 {addr}"));
        } else {
            self.logs.info(format!("当前节点已设置为 {addr}（未运行）"));
        }

        self.events.publish(Event::ConfigChanged);
        self.events.publish(Event::StatusChanged(self.status().await.engine));
        Ok(())
    }

    // ---------------- 测速 ----------------

    /// 启动整批测速，立即返回待测节点数；进度通过事件推送。
    ///
    /// `auto_pick` 为真时，全部测完后自动把评分最高的节点设为当前节点
    /// —— 托盘菜单的「测速并自动选优」走这条路径。选优必须等全部测完，
    /// 否则比较的是新旧混杂的评分。
    pub async fn benchmark_all(
        &self,
        only_missing: bool,
        auto_pick: bool,
    ) -> Result<usize, String> {
        if self.bench_running.swap(true, Ordering::SeqCst) {
            return Err("测速正在进行中".into());
        }

        let nodes = self.cfg.lock().await.nodes.clone();
        let pending: Vec<Node> = if only_missing {
            nodes.into_iter().filter(|n| n.speed_mbps.is_none()).collect()
        } else {
            nodes
        };
        let total = pending.len();

        let this = self.clone();
        tokio::spawn(async move {
            for (i, node) in pending.iter().enumerate() {
                let addr = node.addr();
                // 先通知界面高亮这一行，再开始可能耗时几十秒的下载
                this.events.publish(Event::BenchStarted(BenchStarted {
                    index: i + 1,
                    total,
                    ip: node.ip.clone(),
                }));

                let cfg = this.cfg.lock().await.clone();
                let res = crate::bench::benchmark(&cfg, &addr, &this.client).await;

                this.store_node_result(&node.ip, node.port, &res).await;
                this.events.publish(Event::BenchProgress(BenchProgress {
                    index: i + 1,
                    total,
                    ip: node.ip.clone(),
                    result: res.clone(),
                }));

                match (res.speed_mbps, res.latency_ms) {
                    (Some(speed), Some(lat)) => this.logs.info(format!(
                        "{addr} → {speed:.2} Mbps, {lat} ms（下载 {:.2} MB / {:.1} 秒）",
                        res.bytes as f64 / 1_048_576.0,
                        res.seconds.unwrap_or(0.0)
                    )),
                    _ => this.logs.warn(format!(
                        "{addr} → 失败：{}",
                        res.error.clone().unwrap_or_else(|| "无数据".into())
                    )),
                }
            }

            // 选优放在全部测完之后：此时每个节点才都有最新评分
            if auto_pick {
                this.pick_and_apply_best().await;
            }

            this.bench_running.store(false, Ordering::SeqCst);
            this.events.publish(Event::BenchDone(BenchDone { total }));
            this.logs.info("全部测速完成");
        });

        Ok(total)
    }

    /// 单个节点测速，并把结果写回配置。
    pub async fn benchmark_one(&self, ip: &str, port: u16) -> BenchResult {
        self.events.publish(Event::BenchStarted(BenchStarted {
            index: 1,
            total: 1,
            ip: ip.to_string(),
        }));

        let cfg = self.cfg.lock().await.clone();
        let addr = format!("{ip}:{port}");
        let res = crate::bench::benchmark(&cfg, &addr, &self.client).await;

        // 单个节点测速要等结果，因此这里允许与批量测速并行（各自写不同节点）。
        self.store_node_result(ip, port, &res).await;

        match res.speed_mbps {
            Some(speed) => self.logs.info(format!(
                "{addr} → {speed:.2} Mbps, {} ms（下载 {:.2} MB / {:.1} 秒）",
                res.latency_ms.unwrap_or(0),
                res.bytes as f64 / 1_048_576.0,
                res.seconds.unwrap_or(0.0)
            )),
            None => self.logs.warn(format!(
                "{addr} → 失败：{}",
                res.error.clone().unwrap_or_else(|| "无数据".into())
            )),
        }

        self.events.publish(Event::NodeUpdated(res.clone()));
        res
    }

    async fn store_node_result(&self, ip: &str, port: u16, res: &BenchResult) {
        let mut c = self.cfg.lock().await;
        if let Some(n) = c.nodes.iter_mut().find(|n| n.ip == ip && n.port == port) {
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
        let _ = c.save(self.config_path.as_path());
    }

    /// 把评分最高的节点设为当前节点，返回是否完成处理。
    ///
    /// 与故障切换不同，这里**允许选中的就是当前节点** —— 如果它本来就是最快的，
    /// 保持不动才是正确结果。
    async fn pick_and_apply_best(&self) -> bool {
        let best = {
            let cfg = self.cfg.lock().await;
            crate::config::pick_best(&cfg.nodes, None)
        };

        let Some(addr) = best else {
            self.logs.warn("没有已测速的可用节点，跳过自动选优");
            return false;
        };

        let current = self.cfg.lock().await.upstream_addr();
        if addr == current {
            self.logs.info(format!("{addr} 已是最优节点，保持不变"));
            return true;
        }

        self.logs.info(format!("自动选优：{current} → {addr}"));
        match addr.rsplit_once(':') {
            Some((ip, port)) => {
                let port = port.parse::<u16>().unwrap_or(443);
                // 切换失败不应影响测速流程的收尾，记录后返回即可
                if let Err(e) = self.set_current_node(ip, port).await {
                    self.logs.warn(format!("自动切换节点失败：{e}"));
                    return false;
                }
                true
            }
            None => false,
        }
    }

    // ---------------- 测速链接 ----------------

    /// 选定一个测速链接；若不在列表中则一并加入。返回更新后的链接列表。
    pub async fn set_speed_url(&self, url: &str) -> Result<Vec<String>, String> {
        let url = url.trim().to_string();
        if url.is_empty() {
            return Err("测速链接不能为空".into());
        }
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err("测速链接需以 http:// 或 https:// 开头".into());
        }

        let list = {
            let mut c = self.cfg.lock().await;
            if !c.speed_urls.iter().any(|u| u == &url) {
                c.speed_urls.push(url.clone());
            }
            c.speed_url = url.clone();
            c.save(self.config_path.as_path())
                .map_err(|e| format!("保存配置失败: {e}"))?;
            c.speed_urls.clone()
        };

        self.logs.info(format!("测速链接已设为 {url}"));
        self.events.publish(Event::ConfigChanged);
        Ok(list)
    }

    /// 删除一个测速链接；若删的正是当前链接，则回退到列表首项。
    pub async fn remove_speed_url(&self, url: &str) -> Result<Vec<String>, String> {
        let list = {
            let mut c = self.cfg.lock().await;
            c.speed_urls.retain(|u| u != url);
            if c.speed_urls.is_empty() {
                c.speed_urls
                    .push(crate::config::DEFAULT_SPEED_URL.to_string());
            }
            if c.speed_url.trim().is_empty() || c.speed_url == url {
                c.speed_url = c.speed_urls[0].clone();
            }
            c.save(self.config_path.as_path())
                .map_err(|e| format!("保存配置失败: {e}"))?;
            c.speed_urls.clone()
        };

        self.logs.info(format!("已删除测速链接 {url}"));
        self.events.publish(Event::ConfigChanged);
        Ok(list)
    }

    // ---------------- 出站网卡 ----------------

    /// 设置出站网卡绑定；引擎运行中会重启以生效。
    pub async fn set_egress_interface(&self, iface: &str) -> Result<(), String> {
        let was_running = self.is_running().await;

        let chain_ready = {
            let mut c = self.cfg.lock().await;
            c.egress_interface = iface.to_string();
            c.save(self.config_path.as_path())
                .map_err(|e| format!("保存配置失败: {e}"))?;
            c.chain_enabled && !c.chain_addr.trim().is_empty()
        };

        if was_running {
            self.stop_engine().await;
            self.start_engine().await?;
        }

        let spec = iface.trim();
        if spec.is_empty() {
            self.logs
                .info("出站网卡：自动选择物理网卡（绕过 TUN）");
        } else if spec.eq_ignore_ascii_case("system") {
            self.logs
                .info("出站网卡：跟随系统路由（可能被 TUN 接管）");
        } else {
            self.logs.info(format!("出站网卡已绑定为 {spec}"));
        }

        if !chain_ready && !spec.is_empty() && !spec.eq_ignore_ascii_case("system") {
            self.logs.warn(
                "已绑定物理网卡但未启用 Chain；若该网络需要经代理才能出网，连接会全部超时",
            );
        }

        self.events.publish(Event::ConfigChanged);
        self.events.publish(Event::StatusChanged(self.status().await.engine));
        Ok(())
    }

    // ---------------- 版本更新 ----------------

    /// 查询 GitHub 上的最新 Release 并与当前版本比较。
    ///
    /// 未认证调用有速率限制（每小时 60 次/IP），对本用途足够。
    pub async fn check_update(&self) -> Result<UpdateInfo, String> {
        let resp = self
            .client
            .get(RELEASES_API)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|e| format!("无法连接 GitHub：{e}"))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(format!("GitHub 返回 HTTP {status}"));
        }

        let release: Release = resp
            .json()
            .await
            .map_err(|e| format!("响应解析失败：{e}"))?;

        let current = crate::VERSION.to_string();
        let latest_ver = release.tag_name.trim_start_matches('v').to_string();

        Ok(UpdateInfo {
            has_update: version_tuple(&latest_ver) > version_tuple(&current),
            current,
            latest: release.tag_name,
            url: release.html_url,
            published_at: release.published_at,
            notes: release.body,
        })
    }

    // ---------------- 推送 ----------------

    /// 每秒一次统计推送的循环体，没有订阅者时不产生任何事件。
    ///
    /// 刻意**不自己 spawn**：调用方可能位于非 Tokio 上下文（Tauri 的同步
    /// `setup` 回调就是如此），在那里调用 `tokio::spawn` 会直接 panic，而
    /// release 的 `panic = "abort"` 会让它变成一声不响的闪退。因此由调用方
    /// 挑一个合适的运行时来执行这个 future。
    pub fn stats_ticker(&self) -> impl std::future::Future<Output = ()> + Send + 'static {
        let stats = self.stats.clone();
        let events = self.events.clone();
        async move {
            let mut last = stats.snapshot();
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let now = stats.snapshot();
                let tick = StatsTick::delta(&now, &last);
                last = now;
                if events.receiver_count() > 0 {
                    events.publish(Event::Stats(tick));
                }
            }
        }
    }
}

/// 本仓库的 Release 接口。
const RELEASES_API: &str = "https://api.github.com/repos/sdise/cloudnproxy/releases/latest";

/// GitHub Release 中我们关心的字段。
#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    published_at: String,
    #[serde(default)]
    body: String,
}

/// `check_update` 的返回体。
#[derive(Debug, Clone, Serialize)]
pub struct UpdateInfo {
    /// 是否存在比当前更新的版本
    pub has_update: bool,
    /// 当前版本，如 `0.2.1`
    pub current: String,
    /// 最新版本标签，如 `v0.3.0`
    pub latest: String,
    /// Release 页面地址
    pub url: String,
    /// 发布时间
    pub published_at: String,
    /// Release 说明（Markdown）
    pub notes: String,
}

/// 把 `x.y.z` 解析成可比较的元组；非数字后缀（`1.2.3-beta`）会被截断。
fn version_tuple(v: &str) -> (u32, u32, u32) {
    let mut parts = v.trim().trim_start_matches('v').split('.');
    let mut next = || {
        parts
            .next()
            .map(|p| {
                p.chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse::<u32>()
                    .unwrap_or(0)
            })
            .unwrap_or(0)
    };
    // 必须分三次调用：同一个表达式里连用会触发多重可变借用
    let major = next();
    let minor = next();
    let patch = next();
    (major, minor, patch)
}

#[cfg(test)]
mod tests {
    use super::version_tuple;

    #[test]
    fn parses_versions() {
        assert_eq!(version_tuple("0.2.1"), (0, 2, 1));
        assert_eq!(version_tuple("v1.10.3"), (1, 10, 3));
        assert_eq!(version_tuple("2.0"), (2, 0, 0));
        assert_eq!(version_tuple("bad"), (0, 0, 0));
        // 预发布后缀按主版本比较即可
        assert_eq!(version_tuple("0.3.0-beta.1"), (0, 3, 0));
    }

    #[test]
    fn version_ordering() {
        assert!(version_tuple("0.10.0") > version_tuple("0.9.9"));
        assert!(version_tuple("1.0.0") == version_tuple("1.0.0"));
        assert!(version_tuple("0.2.1") > version_tuple("0.2.0"));
    }
}
