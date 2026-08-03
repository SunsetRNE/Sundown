//! 结构化事件环形缓冲（L3.1 日志数据层）。
//!
//! 参考 AStop/Cerberus v1.6.0 事件时间线模型（见 Sundown-参考项目日志解析.md）：
//! 条目 = 时间 + 级别(性质) + 动作(做了什么) + 主体(谁) + 包名 + 原因 + 消息。
//! 与 Cerberus 的差距是"文本日志 + UI 正则解析"——本模块把关键决策点升级为
//! 结构化事件流，WebUI 后续直接消费 JSON（UI 本刀不做）。
//!
//! 铁律（延续项目纪律）：
//! - 零依赖：JSON 手写（与 toml.rs/state.rs 同哲学）
//! - 容量固定（256），环形覆盖最旧——观测数据可损失，daemon 不可膨胀
//! - 写入 O(1)、瞬时锁，绝不阻塞调用方（决策点可能在主循环/事件线程）
//! - 只读命令 `events [n]` 输出最近 n 条（最旧→最新）

use std::collections::VecDeque;

/// 环形缓冲容量（参考 dex 侧 EventQueue 256 的既有选择）
pub const EVENT_CAPACITY: usize = 256;

/// 级别（参考 Cerberus log_level_* 收敛为项目需要的核心子集）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvLevel {
    Info,
    Event,
    Success,
    Warn,
    Error,
    Timer,
    Report,
}

impl EvLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            EvLevel::Info => "info",
            EvLevel::Event => "event",
            EvLevel::Success => "success",
            EvLevel::Warn => "warn",
            EvLevel::Error => "error",
            EvLevel::Timer => "timer",
            EvLevel::Report => "report",
        }
    }
}

/// 动作（参考 Cerberus log_level_action_*：open/close/freeze/unfreeze/delay + 项目扩展）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvAction {
    Open,     // 前台切换/打开
    Close,    // 退出/force-stop
    Freeze,   // 冻结
    Unfreeze, // 解冻
    Delay,    // grace 计时等待
    Exempt,   // 豁免决策
    Policy,   // 策略加载/重载
    System,   // 系统级（握手/daemon 生命周期）
}

impl EvAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            EvAction::Open => "open",
            EvAction::Close => "close",
            EvAction::Freeze => "freeze",
            EvAction::Unfreeze => "unfreeze",
            EvAction::Delay => "delay",
            EvAction::Exempt => "exempt",
            EvAction::Policy => "policy",
            EvAction::System => "system",
        }
    }
}

/// 主体（参考 Cerberus log_subject_*：app / system；deep_doze 后置）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvSubject {
    App,
    System,
}

impl EvSubject {
    pub fn as_str(&self) -> &'static str {
        match self {
            EvSubject::App => "app",
            EvSubject::System => "system",
        }
    }
}

/// 一条结构化事件
#[derive(Debug, Clone)]
pub struct Event {
    /// 全局序号（EventBuffer.total 单调分配；持久化增量水位的排序键，环形覆盖后仍唯一）
    pub seq: u64,
    /// epoch 秒（日志时间轴排序键）
    pub ts: u64,
    pub level: EvLevel,
    pub action: EvAction,
    pub subject: EvSubject,
    /// 应用包名（subject=App 时必有；System 事件为 None）
    pub pkg: Option<String>,
    /// 原因/触发源（如 grace_expired / wakeup / force_stop）
    pub reason: Option<String>,
    /// 人类可读补充（可选，如"策略已重载"）
    pub msg: Option<String>,
}

impl Event {
    pub fn new(
        level: EvLevel,
        action: EvAction,
        subject: EvSubject,
        pkg: Option<String>,
        reason: Option<String>,
        msg: Option<String>,
    ) -> Self {
        Self {
            seq: 0, // push 时由 EventBuffer 分配
            ts: now_epoch_secs(),
            level,
            action,
            subject,
            pkg,
            reason,
            msg,
        }
    }

    /// JSON 序列化（可选字段省略而非 null——紧凑，前端解析容错）
    pub fn to_json(&self) -> String {
        let mut s = String::with_capacity(112);
        s.push_str("{\"seq\":");
        s.push_str(&self.seq.to_string());
        s.push_str(",\"ts\":");
        s.push_str(&self.ts.to_string());
        s.push_str(",\"level\":\"");
        s.push_str(self.level.as_str());
        s.push_str("\",\"action\":\"");
        s.push_str(self.action.as_str());
        s.push_str("\",\"subject\":\"");
        s.push_str(self.subject.as_str());
        s.push('"');
        if let Some(p) = &self.pkg {
            s.push_str(",\"pkg\":\"");
            s.push_str(&json_escape(p));
            s.push('"');
        }
        if let Some(r) = &self.reason {
            s.push_str(",\"reason\":\"");
            s.push_str(&json_escape(r));
            s.push('"');
        }
        if let Some(m) = &self.msg {
            s.push_str(",\"msg\":\"");
            s.push_str(&json_escape(m));
            s.push('"');
        }
        s.push('}');
        s
    }
}

/// 环形事件缓冲（线程安全由调用方持锁保证——挂在 EngineState 上，引擎锁已持有）
#[derive(Debug)]
pub struct EventBuffer {
    buf: VecDeque<Event>,
    /// 累计产生事件数（覆盖后仍单调递增，诊断丢事件率用）
    pub total: u64,
    /// 已持久化水位（P1⑩ 事件审计：seq <= flushed_seq 的事件已写入 JSONL）
    flushed_seq: u64,
}

impl Default for EventBuffer {
    fn default() -> Self {
        Self {
            buf: VecDeque::with_capacity(EVENT_CAPACITY),
            total: 0,
            flushed_seq: 0,
        }
    }
}

impl EventBuffer {
    /// 写入一条事件；容量满时覆盖最旧。seq 全局单调分配（持久化水位排序键）。
    pub fn push(&mut self, mut e: Event) {
        self.total += 1;
        e.seq = self.total;
        if self.buf.len() >= EVENT_CAPACITY {
            self.buf.pop_front();
        }
        self.buf.push_back(e);
    }

    // ---------------- 持久化审计（P1⑩，对齐 AStop firewall_events 时间线） ----------------

    /// 追加落盘新事件到 JSONL（一行一 JSON；增量水位 flushed_seq，幂等）。
    /// 文件超阈值自动滚动（保留最近 MAX_EVENT_LOGS 份）。失败安全：写失败只留痕不崩溃，
    /// 水位不推进（下轮 tick 重试）；滚动失败不阻塞（继续追加当前文件）。
    pub fn persist_new(&mut self, path: &str) -> usize {
        let mut n = 0usize;
        let mut text = String::new();
        for e in self.buf.iter() {
            if e.seq > self.flushed_seq {
                text.push_str(&e.to_json());
                text.push('\n');
                n += 1;
            }
        }
        if n == 0 {
            return 0;
        }
        if let Err(e) = append_jsonl(path, &text) {
            crate::logw!("事件审计落盘失败: {}（{}）", path, e);
            return 0;
        }
        // 落盘成功才推进水位（失败重试语义）
        if let Some(last) = self.buf.iter().rev().find(|e| e.seq > self.flushed_seq) {
            self.flushed_seq = last.seq;
        }
        n
    }

    /// 当前待落盘事件数（诊断用；0 = 已同步）
    pub fn pending_persist(&self) -> usize {
        self.buf.iter().filter(|e| e.seq > self.flushed_seq).count()
    }

    /// 便捷构造：App 主体事件（包名必有）
    pub fn push_app(
        &mut self,
        level: EvLevel,
        action: EvAction,
        pkg: &str,
        reason: Option<&str>,
        msg: Option<&str>,
    ) {
        self.push(Event::new(
            level,
            action,
            EvSubject::App,
            Some(pkg.to_string()),
            reason.map(|s| s.to_string()),
            msg.map(|s| s.to_string()),
        ));
    }

    /// 便捷构造：System 主体事件
    pub fn push_system(
        &mut self,
        level: EvLevel,
        action: EvAction,
        reason: Option<&str>,
        msg: Option<&str>,
    ) {
        self.push(Event::new(
            level,
            action,
            EvSubject::System,
            None,
            reason.map(|s| s.to_string()),
            msg.map(|s| s.to_string()),
        ));
    }

    /// 当前缓冲内条数
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// 最近 limit 条（最旧→最新）；limit=0 或超容量 → 全部
    pub fn recent(&self, limit: usize) -> Vec<&Event> {
        let take = if limit == 0 { self.buf.len() } else { limit.min(self.buf.len()) };
        self.buf.iter().skip(self.buf.len() - take).collect()
    }

    /// 最近 limit 条 JSON 数组（`[{...},{...}]`）
    pub fn to_json(&self, limit: usize) -> String {
        let events = self.recent(limit);
        let mut s = String::with_capacity(64 + events.len() * 96);
        s.push('[');
        for (i, e) in events.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&e.to_json());
        }
        s.push(']');
        s
    }
}

/// JSONL 滚动保留份数（events.jsonl + events.1..N；超出删最旧）
const MAX_EVENT_LOGS: usize = 3;
/// 单份 JSONL 滚动阈值（2MB ≈ 2 万+条事件；观测数据可损失，daemon 不膨胀）
const EVENT_LOG_ROTATE_BYTES: u64 = 2 * 1024 * 1024;

/// 追加写 JSONL：文件超阈值先滚动（events.jsonl → events.1.jsonl → … → 删最旧），
/// 再 append。滚动/写失败返回 Err（调用方留痕，不崩溃）。
fn append_jsonl(path: &str, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    // 滚动检查（仅当前文件超阈值时）
    if let Ok(md) = std::fs::metadata(path) {
        if md.len() >= EVENT_LOG_ROTATE_BYTES {
            rotate_jsonl(path);
        }
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    f.write_all(text.as_bytes())
}

/// 滚动：events.jsonl → events.1.jsonl → events.2.jsonl → 删 events.3.jsonl
/// （只保留 MAX_EVENT_LOGS 份历史）。失败静默（下次滚动重试；当前文件继续追加）。
fn rotate_jsonl(path: &str) {
    for i in (1..MAX_EVENT_LOGS).rev() {
        let from = format!("{}.{}", path, i);
        let to = format!("{}.{}", path, i + 1);
        let _ = std::fs::rename(&from, &to);
    }
    let _ = std::fs::rename(path, format!("{}.1", path));
    crate::logw!("事件审计日志滚动: {}", path);
}

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// JSON 字符串转义（事件字段仅包名/reason/msg，控制字符罕见；覆盖引号/反斜杠即可）
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_capacity() {
        let mut b = EventBuffer::default();
        for i in 0..(EVENT_CAPACITY + 50) {
            b.push_app(
                EvLevel::Event,
                EvAction::Freeze,
                &format!("com.pkg.{}", i),
                Some("grace_expired"),
                None,
            );
        }
        assert_eq!(b.len(), EVENT_CAPACITY);
        assert_eq!(b.total, (EVENT_CAPACITY + 50) as u64);
        // 最旧的 50 条被覆盖，最新一条保留
        let recent = b.recent(1);
        assert_eq!(recent[0].pkg.as_deref(), Some("com.pkg.305"));
        // 最旧一条 = 第 50 条（0..50 被覆盖）
        let all = b.recent(0);
        assert_eq!(all[0].pkg.as_deref(), Some("com.pkg.50"));
    }

    #[test]
    fn json_format() {
        let mut b = EventBuffer::default();
        b.push_app(EvLevel::Success, EvAction::Freeze, "com.tencent.mm", Some("grace_expired"), Some("L3 冻结"));
        b.push_system(EvLevel::Report, EvAction::System, Some("daemon_start"), Some("v0.4.9-l3"));
        let j = b.to_json(0);
        assert!(j.starts_with('[') && j.ends_with(']'));
        assert!(j.contains("\"level\":\"success\""));
        assert!(j.contains("\"action\":\"freeze\""));
        assert!(j.contains("\"subject\":\"app\""));
        assert!(j.contains("\"pkg\":\"com.tencent.mm\""));
        assert!(j.contains("\"reason\":\"grace_expired\""));
        assert!(j.contains("\"msg\":\"L3 冻结\""));
        assert!(j.contains("\"action\":\"system\""));
        assert!(j.contains("\"subject\":\"system\""));
        // 无 pkg 的 system 事件不含 pkg 字段
        assert!(!j.contains("\"pkg\":\"v0.4.9-l3\""));
        // limit 截取
        b.push_app(EvLevel::Event, EvAction::Unfreeze, "com.x", Some("wakeup"), None);
        let j2 = b.to_json(1);
        assert_eq!(j2.matches("\"ts\"").count(), 1);
        assert!(j2.contains("\"pkg\":\"com.x\""));
    }

    #[test]
    fn json_escape_chars() {
        let mut b = EventBuffer::default();
        b.push_app(EvLevel::Info, EvAction::Exempt, "a\"b\\c", None, Some("行\n\t"));
        let j = b.to_json(0);
        assert!(j.contains("\"pkg\":\"a\\\"b\\\\c\""));
        assert!(j.contains("行\\n\\t"));
    }

    #[test]
    fn seq_assigned_and_serialized() {
        let mut b = EventBuffer::default();
        b.push_app(EvLevel::Event, EvAction::Freeze, "com.a", Some("grace_expired"), None);
        b.push_app(EvLevel::Event, EvAction::Freeze, "com.b", Some("grace_expired"), None);
        let j = b.to_json(0);
        // seq 从 1 开始单调
        assert!(j.contains("\"seq\":1"));
        assert!(j.contains("\"seq\":2"));
        let all = b.recent(0);
        assert_eq!(all[0].seq, 1);
        assert_eq!(all[1].seq, 2);
    }

    #[test]
    fn persist_new_incremental_and_idempotent() {
        let dir = std::env::temp_dir().join("sundown_events_test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        let p = path.to_str().unwrap();

        let mut b = EventBuffer::default();
        b.push_app(EvLevel::Event, EvAction::Freeze, "com.a", Some("grace_expired"), None);
        b.push_app(EvLevel::Event, EvAction::Unfreeze, "com.b", Some("wakeup"), None);

        // 首次落盘 2 条
        assert_eq!(b.persist_new(p), 2);
        assert_eq!(b.pending_persist(), 0);
        let txt = std::fs::read_to_string(&p).unwrap();
        assert_eq!(txt.lines().count(), 2);
        assert!(txt.lines().all(|l| l.starts_with('{') && l.ends_with('}')));

        // 幂等：无新事件不再写
        assert_eq!(b.persist_new(p), 0);
        assert_eq!(std::fs::read_to_string(&p).unwrap().lines().count(), 2);

        // 新增 1 条 → 只追加 1 条
        b.push_app(EvLevel::Warn, EvAction::Exempt, "com.c", Some("tick_exempt"), None);
        assert_eq!(b.persist_new(p), 1);
        assert_eq!(b.pending_persist(), 0);
        let txt2 = std::fs::read_to_string(&p).unwrap();
        assert_eq!(txt2.lines().count(), 3);
        assert!(txt2.lines().last().unwrap().contains("\"pkg\":\"com.c\""));

        // 环形覆盖后水位仍正确：灌满 +50，落盘应只写未覆盖的新事件
        for i in 0..(EVENT_CAPACITY + 50) {
            b.push_app(EvLevel::Event, EvAction::Freeze, &format!("com.pkg.{}", i), None, None);
        }
        let n = b.persist_new(p);
        assert!(n > 0);
        assert_eq!(b.pending_persist(), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
