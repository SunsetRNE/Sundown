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
        let mut s = String::with_capacity(96);
        s.push_str("{\"ts\":");
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
}

impl Default for EventBuffer {
    fn default() -> Self {
        Self {
            buf: VecDeque::with_capacity(EVENT_CAPACITY),
            total: 0,
        }
    }
}

impl EventBuffer {
    /// 写入一条事件；容量满时覆盖最旧
    pub fn push(&mut self, e: Event) {
        self.total += 1;
        if self.buf.len() >= EVENT_CAPACITY {
            self.buf.pop_front();
        }
        self.buf.push_back(e);
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
}
