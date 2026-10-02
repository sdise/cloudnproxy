//! 控制台事件总线：引擎与各前端之间的单向通知通道。
//!
//! 实时推送（统计、日志、测速进度、状态变化）全部经由这里，前端只需按
//! `type` 分发：
//!
//! - Tauri 侧把每个事件转成同名的 `emit`；
//! - Web 控制台把它编码成 SSE 的 `data:` 帧（见 `t5-web`）。
//!
//! 这样业务层不必知道对端是哪种前端，也不必为两种前端各写一份推送代码。
//! 序列化格式固定为 `{"type":"bench-progress","payload":{…}}`，与 Tauri 的
//! `emit(name, payload)` 一一对应。

use crate::bench::BenchResult;
use crate::config::Node;
use crate::engine::EngineStatus;
use crate::logbuf::LogLine;
use crate::stats::StatsSnapshot;
use serde::Serialize;
use tokio::sync::broadcast;

/// 通道容量。1 Hz 的统计加上连接日志，1024 条足够吸收前端短暂卡顿；
/// 超出时落后的订阅者会收到 `Lagged` 并跳过缺失的帧，而不是把内存撑大。
pub const EVENT_CHANNEL_CAPACITY: usize = 1024;

/// 每秒推送一次的实时统计。`*_rate` 是「上一秒的增量」，不是累计值。
#[derive(Debug, Clone, Default, Serialize)]
pub struct StatsTick {
    pub up_rate: u64,
    pub down_rate: u64,
    pub up_bytes: u64,
    pub down_bytes: u64,
    pub conns: i64,
    pub sessions: u64,
}

impl StatsTick {
    /// 用「当前快照」与「上一秒快照」构造一次增量。
    pub fn delta(now: &StatsSnapshot, last: &StatsSnapshot) -> Self {
        Self {
            up_rate: now.up_bytes.saturating_sub(last.up_bytes),
            down_rate: now.down_bytes.saturating_sub(last.down_bytes),
            up_bytes: now.up_bytes,
            down_bytes: now.down_bytes,
            conns: now.conns,
            sessions: now.sessions,
        }
    }
}

/// 单个节点测速完成。
#[derive(Debug, Clone, Serialize)]
pub struct BenchProgress {
    /// 已完成数量（从 1 开始）
    pub index: usize,
    pub total: usize,
    pub ip: String,
    pub result: BenchResult,
}

/// 整批测速结束。
#[derive(Debug, Clone, Serialize)]
pub struct BenchDone {
    pub total: usize,
}

/// 推送给前端的事件。序列化后形如 `{"type":"stats","payload":{…}}`。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", content = "payload", rename_all = "kebab-case")]
pub enum Event {
    /// 每秒一次的实时统计
    Stats(StatsTick),
    /// 一条日志
    Log(LogLine),
    /// 批量测速的单个节点进度
    BenchProgress(BenchProgress),
    /// 批量测速结束
    BenchDone(BenchDone),
    /// 节点列表变化（解析域名后）
    NodesChanged(Vec<Node>),
    /// 单个节点数据更新
    NodeUpdated(BenchResult),
    /// 引擎启停
    StatusChanged(EngineStatus),
    /// 配置已变更（无内容，前端各自重新拉取）
    ConfigChanged,
}

impl Event {
    /// 与 Tauri `emit` 一致的名称，便于前端用同一张事件表分发。
    pub fn name(&self) -> &'static str {
        match self {
            Event::Stats(_) => "stats",
            Event::Log(_) => "log",
            Event::BenchProgress(_) => "bench-progress",
            Event::BenchDone(_) => "bench-done",
            Event::NodesChanged(_) => "nodes-changed",
            Event::NodeUpdated(_) => "node-updated",
            Event::StatusChanged(_) => "status-changed",
            Event::ConfigChanged => "config-changed",
        }
    }
}

/// 事件总线句柄，克隆成本极低，可在任意任务中发布。
#[derive(Clone, Default)]
pub struct EventBus {
    tx: broadcast::Sender<Event>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self { tx }
    }

    /// 发布事件。没有订阅者时静默丢弃（无 GUI、无 Web 控制台时属正常情况）。
    pub fn publish(&self, event: Event) {
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    /// 当前订阅者数量，用于判断是否值得推送高频事件。
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_delta_clamps_on_counter_reset() {
        let now = StatsSnapshot {
            up_bytes: 100,
            down_bytes: 50,
            conns: 1,
            sessions: 2,
        };
        // 上一秒的累计值比现在还大（例如计数器被重置）时不应回绕成天文数字
        let last = StatsSnapshot {
            up_bytes: 900,
            down_bytes: 900,
            conns: 0,
            sessions: 0,
        };
        let tick = StatsTick::delta(&now, &last);
        assert_eq!(tick.up_rate, 0);
        assert_eq!(tick.down_rate, 0);
    }

    #[test]
    fn events_serialize_with_type_and_payload() {
        let json = serde_json::to_string(&Event::BenchDone(BenchDone { total: 3 })).unwrap();
        assert_eq!(json, r#"{"type":"bench-done","payload":{"total":3}}"#);

        let json = serde_json::to_string(&Event::ConfigChanged).unwrap();
        assert_eq!(json, r#"{"type":"config-changed"}"#);
    }

    #[test]
    fn event_names_match_tauri_emit_names() {
        assert_eq!(Event::BenchProgress(BenchProgress {
            index: 1,
            total: 2,
            ip: "1.1.1.1".into(),
            result: BenchResult::default(),
        }).name(), "bench-progress");
        assert_eq!(Event::ConfigChanged.name(), "config-changed");
    }
}
