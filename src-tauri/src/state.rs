//! 应用全局状态。
//!
//! 所有需要在后台任务中共享的部分都包成 `Arc`，这样任务可以 clone 出所有权，
//! 不必跨 `await` 持有 `tauri::State` 的借用。

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use t5_core::{Config, EngineHandle, LogSink, Stats};
use tokio::sync::Mutex;

pub struct AppState {
    pub cfg: Arc<Mutex<Config>>,
    pub engine: Arc<Mutex<Option<EngineHandle>>>,
    pub bench_running: Arc<AtomicBool>,
    pub logs: LogSink,
    pub stats: Stats,
    pub client: reqwest::Client,
    pub config_path: Arc<PathBuf>,
}

impl AppState {
    pub fn new(cfg: Config, logs: LogSink, stats: Stats, config_path: PathBuf) -> Self {
        Self {
            cfg: Arc::new(Mutex::new(cfg)),
            engine: Arc::new(Mutex::new(None)),
            bench_running: Arc::new(AtomicBool::new(false)),
            logs,
            stats,
            client: t5_core::resolver::http_client(),
            config_path: Arc::new(config_path),
        }
    }
}

/// 停止引擎（幂等）。
pub async fn stop_engine(state: &AppState) {
    let handle = {
        let mut guard = state.engine.lock().await;
        guard.take()
    };
    if let Some(h) = handle {
        h.shutdown().await;
    }
}

/// 启动引擎；已在运行则直接返回。
pub async fn start_engine(state: &AppState) -> Result<(), String> {
    let already = {
        let guard = state.engine.lock().await;
        guard.as_ref().map(|h| h.is_running()).unwrap_or(false)
    };
    if already {
        return Ok(());
    }
    let cfg = state.cfg.lock().await.clone();
    let handle = t5_core::engine::start(cfg, state.logs.clone(), state.stats.clone())
        .await
        .map_err(|e| e.to_string())?;
    *state.engine.lock().await = Some(handle);
    Ok(())
}
