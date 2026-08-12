//! L3 声明式规则引擎（conf/rules.toml）：快速应对层核心（缺口补入清单 B3）。
//!
//! 价值：App 设计缺陷（疯狂唤醒/内存爆炸/后台驻留）→ 写规则热加载，**不重新编译 dex**。
//! 对齐 AStop autostart_rules 字段族：rule_id / priority / applies_to / condition /
//! action / expires_at / throttle。
//!
//! 规则段（[rules."<rule_id>"]）：
//! ```toml
//! [rules."suppress-qq-push"]
//! priority   = 100
//! applies_to = [ "com.tencent.mobileqq" ]
//! condition  = "wakeup"                       # always | leave | wakeup | focus
//! source     = "broadcast"                    # 可选：唤醒源限定（condition=wakeup）
//! action     = "suppress"                     # suppress | exempt | freeze | discard
//! throttle   = 60                             # 命中后窗口内不再命中（秒；0=关闭）
//! after_seconds = 120                         # 仅 discard：冻结多久后丢弃
//! expires_at = "2026-12-31"                   # 可选：过期日期（含当日生效）
//! ```
//!
//! 动作语义（engine 决策面插入点）：
//!   suppress —— 抑制唤醒：on_wakeup 命中则不解冻不取消 grace（风暴压制）
//!   exempt   —— 豁免：decide_leave 命中则永不冻结（等效条件白名单）
//!   freeze   —— 立即冻结：decide_leave 命中则跳过 grace 立即冻结（等效条件 force）
//!   discard  —— 冻结后按 after_seconds 丢弃（SIGKILL 释放内存；缺省 = 全局超时）
//!
//! 优先级链（engine 决策面）：critical > 系统组件 > 白名单/VPN > 豁免链 >
//!   规则引擎（exempt/freeze）> force > grace；suppress 在唤醒门控之前。
//! 失败安全：解析失败 → 调用方保留旧表；未知键/未知取值 → 警告不致命（前向兼容）。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::toml::{parse, TomlEntry, TomlValue};
use crate::{logw, paths};

/// 规则触发条件
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleCondition {
    /// 任何判定点都匹配（常驻规则）
    Always,
    /// 旧前台离开（decide_leave）
    Leave,
    /// 唤醒事件（on_wakeup）
    Wakeup,
    /// 前台进入（on_focus）
    Focus,
}

impl RuleCondition {
    fn parse(s: &str) -> Option<RuleCondition> {
        match s {
            "always" => Some(RuleCondition::Always),
            "leave" => Some(RuleCondition::Leave),
            "wakeup" => Some(RuleCondition::Wakeup),
            "focus" => Some(RuleCondition::Focus),
            _ => None,
        }
    }

    /// 观测面序列化（rules list --detail / 事件留痕；当前引擎内部按 RuleAction 分派）
    #[allow(dead_code)]
    pub fn as_str(&self) -> &'static str {
        match self {
            RuleCondition::Always => "always",
            RuleCondition::Leave => "leave",
            RuleCondition::Wakeup => "wakeup",
            RuleCondition::Focus => "focus",
        }
    }
}

/// 规则动作
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleAction {
    /// 抑制唤醒（风暴压制）
    Suppress,
    /// 豁免（永不冻结）
    Exempt,
    /// 立即冻结（跳过 grace）
    Freeze,
    /// 冻结后丢弃（SIGKILL 释放内存）
    Discard,
}

impl RuleAction {
    fn parse(s: &str) -> Option<RuleAction> {
        match s {
            "suppress" => Some(RuleAction::Suppress),
            "exempt" => Some(RuleAction::Exempt),
            "freeze" => Some(RuleAction::Freeze),
            "discard" => Some(RuleAction::Discard),
            _ => None,
        }
    }

    /// 观测面序列化（rules list --detail / 事件留痕；当前引擎内部按 RuleAction 分派）
    #[allow(dead_code)]
    pub fn as_str(&self) -> &'static str {
        match self {
            RuleAction::Suppress => "suppress",
            RuleAction::Exempt => "exempt",
            RuleAction::Freeze => "freeze",
            RuleAction::Discard => "discard",
        }
    }
}

/// 单条规则
#[derive(Debug, Clone)]
pub struct Rule {
    pub id: String,
    /// 优先级（越大越先；同优先级按定义顺序，先定义先命中）
    pub priority: i64,
    /// 目标包名列表（空 = 所有包；"*" = 通配全部）
    pub applies_to: Vec<String>,
    pub condition: RuleCondition,
    /// 唤醒源限定（仅 condition=wakeup 时语义生效；None = 不限定）
    pub source: Option<String>,
    pub action: RuleAction,
    /// 命中后窗口内不再命中（秒；0 = 关闭）
    pub throttle_seconds: u64,
    /// 仅 discard：冻结多久后丢弃（None = 全局 frozen_timeout）
    pub after_seconds: Option<u64>,
    /// 过期日期 (y, m, d)；含当日仍生效，次日失效
    pub expires_at: Option<(u32, u32, u32)>,
    /// pkg → 上次命中时刻（节流判定）
    pub last_hit: HashMap<String, Instant>,
    /// 内部标记：action 键是否显式声明（解析收尾剔除未声明的规则）
    action_set: bool,
}

/// 规则命中快照（owned，避免 &mut 借用冲突）
#[derive(Debug, Clone)]
pub struct RuleHit {
    pub id: String,
    pub action: RuleAction,
    pub after_seconds: Option<u64>,
}

/// 规则表（rules.toml 解析结果；空表 = 未配置规则）
#[derive(Debug, Clone, Default)]
pub struct RuleTable {
    pub rules: Vec<Rule>,
    /// rules.toml 修订号（mtime 秒）
    pub revision: u64,
    /// 累计命中次数（status 观测；进程生命周期内累计）
    pub hits: u64,
}

impl RuleTable {
    /// 从 TOML 文本构建规则表；Err = 解析失败（调用方保留旧表）
    pub fn from_toml(src: &str, revision: u64) -> Result<RuleTable, String> {
        let entries = parse(src).map_err(|(lineno, msg)| format!("第 {} 行: {}", lineno, msg))?;
        let mut t = RuleTable { rules: Vec::new(), revision, hits: 0 };
        for e in &entries {
            let section = e.table.join(".");
            if let Some(id) = section.strip_prefix("rules.") {
                if id.is_empty() {
                    logw!("规则 [rules] 裸段（缺 id）忽略: {} = {}", e.key, debug_val(&e.value));
                    continue;
                }
                // 同 id 多键合并到一条（TOML 子集无内联表，多行分段属正常写法）
                if !t.rules.iter().any(|r| r.id == id) {
                    t.rules.push(Rule {
                        id: id.to_string(),
                        priority: 0,
                        applies_to: Vec::new(),
                        condition: RuleCondition::Always,
                        source: None,
                        action: RuleAction::Suppress,
                        throttle_seconds: 0,
                        after_seconds: None,
                        expires_at: None,
                        last_hit: HashMap::new(),
                        action_set: false,
                    });
                }
                let r = t.rules.iter_mut().find(|r| r.id == id).unwrap();
                let val = &e.value;
                match e.key.as_str() {
                    "priority" => r.priority = int_of(val, e),
                    "applies_to" => r.applies_to = str_array_of(val, e),
                    "condition" => match str_of(val, e) {
                        Some(s) => match RuleCondition::parse(&s) {
                            Some(c) => r.condition = c,
                            None => logw!("规则 [rules.{}] condition 未知（忽略，用 always）: {}", id, s),
                        },
                        None => logw!("规则 [rules.{}] condition 类型错误（忽略）", id),
                    },
                    "source" => match str_of(val, e) {
                        Some(s) => {
                            if matches!(s.as_str(), "broadcast" | "service" | "pendingintent") {
                                r.source = Some(s);
                            } else {
                                logw!("规则 [rules.{}] source 未知（忽略）: {}", id, s);
                            }
                        }
                        None => logw!("规则 [rules.{}] source 类型错误（忽略）", id),
                    },
                    "action" => match str_of(val, e) {
                        Some(s) => match RuleAction::parse(&s) {
                            Some(a) => {
                                r.action = a;
                                r.action_set = true;
                            }
                            None => logw!("规则 [rules.{}] action 未知（忽略，规则失效）: {}", id, s),
                        },
                        None => logw!("规则 [rules.{}] action 类型错误（忽略，规则失效）", id),
                    },
                    "throttle" => r.throttle_seconds = int_of(val, e).max(0) as u64,
                    "after_seconds" => r.after_seconds = Some(int_of(val, e).max(0) as u64),
                    "expires_at" => match str_of(val, e) {
                        Some(s) => match parse_date(&s) {
                            Some(d) => r.expires_at = Some(d),
                            None => logw!("规则 [rules.{}] expires_at 格式错误（应为 YYYY-MM-DD，忽略）: {}", id, s),
                        },
                        None => logw!("规则 [rules.{}] expires_at 类型错误（忽略）", id),
                    },
                    k => logw!("规则 [rules.{}] 未知键（忽略）: {} = {}", id, k, debug_val(val)),
                }
            } else if !section.is_empty() {
                logw!("规则未知段（忽略）: [{}] {} = {}", section, e.key, debug_val(&e.value));
            }
        }
        // action 必填：未显式声明 action 的规则剔除（保底 Suppress 占位不可静默生效）
        let before = t.rules.len();
        t.rules.retain(|r| {
            if !r.action_set {
                logw!("规则 [rules.{}] 缺少 action（忽略整条规则）——必须声明 action = suppress|exempt|freeze|discard", r.id);
                false
            } else {
                true
            }
        });
        if t.rules.len() != before {
            logw!("规则表收尾：剔除 {} 条缺少 action 的规则", before - t.rules.len());
        }
        Ok(t)
    }

    /// 从磁盘加载 rules.toml；缺失返回 None（调用方保留旧表）
    pub fn load() -> Option<(RuleTable, String)> {
        let text = std::fs::read_to_string(paths::RULES_FILE).ok()?;
        let revision = std::fs::metadata(paths::RULES_FILE)
            .ok()
            .and_then(|m| m.modified().ok())
            .map(|t| t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0))
            .unwrap_or(0);
        match RuleTable::from_toml(&text, revision) {
            Ok(t) => Some((t, text)),
            Err(e) => {
                logw!("规则解析失败（保留旧表）: {} —— {}", paths::RULES_FILE, e);
                None
            }
        }
    }

    /// 只读匹配（热更新对账 / status 观测用；不更新节流状态）
    pub fn peek(&self, pkg: &str, cond: RuleCondition, source: Option<&str>, now: Instant) -> Option<RuleHit> {
        self.find_index(pkg, cond, source, now).map(|i| {
            let r = &self.rules[i];
            RuleHit { id: r.id.clone(), action: r.action, after_seconds: r.after_seconds }
        })
    }

    /// 命中匹配（执行点用；命中后更新节流状态）
    pub fn hit(&mut self, pkg: &str, cond: RuleCondition, source: Option<&str>, now: Instant) -> Option<RuleHit> {
        let idx = self.find_index(pkg, cond, source, now)?;
        self.hits += 1;
        let r = &mut self.rules[idx];
        if r.throttle_seconds > 0 {
            r.last_hit.insert(pkg.to_string(), now);
        }
        Some(RuleHit { id: r.id.clone(), action: r.action, after_seconds: r.after_seconds })
    }

    /// 最高优先级命中规则索引（同优先级按定义顺序）
    fn find_index(&self, pkg: &str, cond: RuleCondition, source: Option<&str>, now: Instant) -> Option<usize> {
        let mut idxs: Vec<usize> = (0..self.rules.len())
            .filter(|&i| self.rules[i].matches(pkg, cond, source, now))
            .collect();
        idxs.sort_by(|&a, &b| {
            self.rules[b]
                .priority
                .cmp(&self.rules[a].priority)
                .then(a.cmp(&b))
        });
        idxs.into_iter().next()
    }

    /// 规则条数（status 观测）
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// 稳定排序的规则 id 列表（rules list 输出）
    pub fn ids(&self) -> Vec<String> {
        let mut v: Vec<String> = self.rules.iter().map(|r| r.id.clone()).collect();
        v.sort();
        v
    }
}

impl Rule {
    /// 规则是否命中（applies_to + 条件 + 源限定 + 过期 + 节流 全链路）
    fn matches(&self, pkg: &str, cond: RuleCondition, source: Option<&str>, now: Instant) -> bool {
        // applies_to：空或含 "*" = 全部；否则精确命中
        if !self.applies_to.is_empty() && !self.applies_to.iter().any(|p| p == "*" || p == pkg) {
            return false;
        }
        // 条件：Always 通吃；其余需等于事件条件
        if self.condition != RuleCondition::Always && self.condition != cond {
            return false;
        }
        // wakeup 源限定
        if let Some(s) = &self.source {
            if source != Some(s.as_str()) {
                return false;
            }
        }
        // 过期（含当日仍生效；次日失效；本地时间获取失败 → 视为未过期，保守生效）
        if let Some(exp) = self.expires_at {
            if let Some(today) = today_ymd() {
                if today >= exp {
                    return false;
                }
            }
        }
        // 节流窗口内不再命中
        if self.throttle_seconds > 0 {
            if let Some(&last) = self.last_hit.get(pkg) {
                if now.duration_since(last) < Duration::from_secs(self.throttle_seconds) {
                    return false;
                }
            }
        }
        true
    }
}

/// 当前本地日期 (y, m, d)；失败 None。libc localtime_r（零依赖，与 engine now_minutes 同哲学）
fn today_ymd() -> Option<(u32, u32, u32)> {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        if t < 0 {
            return None;
        }
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&t, &mut tm).is_null() {
            return None;
        }
        Some((tm.tm_year as u32 + 1900, tm.tm_mon as u32 + 1, tm.tm_mday as u32))
    }
}

/// 解析 "YYYY-MM-DD" → (y, m, d)；格式错误/越界 → None
fn parse_date(s: &str) -> Option<(u32, u32, u32)> {
    let parts: Vec<&str> = s.trim().split('-').collect();
    if parts.len() != 3 {
        return None;
    }
    let y: u32 = parts[0].parse().ok()?;
    let m: u32 = parts[1].parse().ok()?;
    let d: u32 = parts[2].parse().ok()?;
    if m < 1 || m > 12 || d < 1 || d > 31 {
        return None;
    }
    Some((y, m, d))
}

// ---- 值提取（与 policy.rs / preset.rs 同构，失败安全） ----

fn str_of(v: &TomlValue, e: &TomlEntry) -> Option<String> {
    match v {
        TomlValue::Str(s) => Some(s.clone()),
        _ => {
            logw!("规则键类型错误（期望字符串）: {} = {}", e.key, debug_val(v));
            None
        }
    }
}

fn int_of(v: &TomlValue, e: &TomlEntry) -> i64 {
    match v {
        TomlValue::Int(n) => *n,
        _ => {
            logw!("规则键类型错误（用 0）: {} = {}", e.key, debug_val(v));
            0
        }
    }
}

fn str_array_of(v: &TomlValue, e: &TomlEntry) -> Vec<String> {
    match v {
        TomlValue::StrArray(items) => items.clone(),
        _ => {
            logw!("规则键类型错误（用空数组）: {} = {}", e.key, debug_val(v));
            Vec::new()
        }
    }
}

fn debug_val(v: &TomlValue) -> String {
    match v {
        TomlValue::Str(s) => format!("\"{}\"", s),
        TomlValue::Bool(b) => b.to_string(),
        TomlValue::Int(n) => n.to_string(),
        TomlValue::StrArray(a) => format!("[{}]", a.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> Instant {
        Instant::now()
    }

    #[test]
    fn parse_rules_ok() {
        let src = r#"
[rules."suppress-qq-push"]
priority = 100
applies_to = [ "com.tencent.mobileqq" ]
condition = "wakeup"
source = "broadcast"
action = "suppress"
throttle = 60

[rules."protect-navi"]
priority = 200
applies_to = [ "com.example.navi" ]
action = "exempt"

[rules."kill-greedy"]
condition = "leave"
action = "freeze"
expires_at = "2026-12-31"
after_seconds = 300
"#;
        let t = RuleTable::from_toml(src, 42).unwrap();
        assert_eq!(t.rules.len(), 3);
        assert_eq!(t.revision, 42);

        let qq = t.rules.iter().find(|r| r.id == "suppress-qq-push").unwrap();
        assert_eq!(qq.priority, 100);
        assert_eq!(qq.applies_to, vec!["com.tencent.mobileqq"]);
        assert_eq!(qq.condition, RuleCondition::Wakeup);
        assert_eq!(qq.source.as_deref(), Some("broadcast"));
        assert_eq!(qq.action, RuleAction::Suppress);
        assert_eq!(qq.throttle_seconds, 60);
        assert!(qq.expires_at.is_none());

        let navi = t.rules.iter().find(|r| r.id == "protect-navi").unwrap();
        assert_eq!(navi.priority, 200);
        assert_eq!(navi.condition, RuleCondition::Always); // 缺省 always
        assert_eq!(navi.action, RuleAction::Exempt);
        assert!(navi.applies_to.iter().any(|p| p == "com.example.navi"));

        let greedy = t.rules.iter().find(|r| r.id == "kill-greedy").unwrap();
        assert_eq!(greedy.action, RuleAction::Freeze);
        assert_eq!(greedy.condition, RuleCondition::Leave);
        assert_eq!(greedy.expires_at, Some((2026, 12, 31)));
        assert_eq!(greedy.after_seconds, Some(300));
        // 缺省字段
        assert_eq!(greedy.priority, 0);
        assert_eq!(greedy.throttle_seconds, 0);
        assert!(greedy.applies_to.is_empty()); // 空 = 所有包
    }

    #[test]
    fn parse_rules_bad() {
        // 语法错误 → Err（调用方保留旧表）
        assert!(RuleTable::from_toml("k = ", 0).is_err());
        assert!(RuleTable::from_toml("[rules\n", 0).is_err());
        // 未知键/未知取值 → 不致命（前向兼容）
        let t = RuleTable::from_toml(
            "[rules.\"x\"]\naction = \"suppress\"\nfuture_key = 1\ncondition = \"turbo\"",
            1,
        )
        .unwrap();
        assert_eq!(t.rules.len(), 1);
        assert_eq!(t.rules[0].action, RuleAction::Suppress);
        assert_eq!(t.rules[0].condition, RuleCondition::Always); // 未知回落 always
        // 未知 action → 警告不致命（解析不报错），但动作语义不成立 → 整条剔除
        // （与"缺少 action"同语义：action 必填且必须可解析，保底 Suppress 不可静默生效）
        let t2 = RuleTable::from_toml("[rules.\"y\"]\naction = \"nuke\"", 2).unwrap();
        assert!(t2.rules.is_empty());
        // 非 rules 段忽略
        let t3 = RuleTable::from_toml("[other]\nfoo = 1", 3).unwrap();
        assert!(t3.rules.is_empty());
        // 裸段忽略
        let t4 = RuleTable::from_toml("[rules]\nfoo = 1", 4).unwrap();
        assert!(t4.rules.is_empty());
    }

    #[test]
    fn match_applies_to() {
        let src = r#"
[rules."all"]
action = "exempt"

[rules."qq-only"]
applies_to = [ "com.tencent.mobileqq" ]
action = "freeze"

[rules."star"]
applies_to = [ "*" ]
action = "suppress"
"#;
        let t = RuleTable::from_toml(src, 1).unwrap();
        // 空 applies_to = 所有包；同 priority 按定义顺序（all 先定义 → 命中 all）
        let h = t.peek("com.anything", RuleCondition::Leave, None, now()).unwrap();
        assert_eq!(h.id, "all");
        assert_eq!(h.action, RuleAction::Exempt);
        // 精确命中（同 priority 仍按定义顺序：all 先定义）
        let h2 = t.peek("com.tencent.mobileqq", RuleCondition::Leave, None, now()).unwrap();
        assert_eq!(h2.id, "all");
        // priority 生效：qq-only priority=10 → freeze 覆盖
        let t2 = RuleTable::from_toml(
            "[rules.\"qq-only\"]\napplies_to = [\"com.tencent.mobileqq\"]\naction = \"freeze\"\npriority = 10\n\n[rules.\"all\"]\naction = \"exempt\"",
            1,
        )
        .unwrap();
        let h3 = t2.peek("com.tencent.mobileqq", RuleCondition::Leave, None, now()).unwrap();
        assert_eq!(h3.action, RuleAction::Freeze);
        // "*" 通配 = 所有包（star 定义最后，同 priority 不敌 all）
        let h4 = t.peek("com.other.app", RuleCondition::Leave, None, now()).unwrap();
        assert_eq!(h4.id, "all");
        // 未命中场景：条件不匹配（leave 事件不命中 wakeup 条件规则）
        let t3 = RuleTable::from_toml(
            "[rules.\"w\"]\napplies_to = [\"com.x\"]\ncondition = \"wakeup\"\naction = \"suppress\"",
            1,
        )
        .unwrap();
        assert!(t3.peek("com.x", RuleCondition::Leave, None, now()).is_none());
    }

    #[test]
    fn parse_rules_missing_action() {
        // 缺少 action → 整条剔除（action 必填）
        let t = RuleTable::from_toml(
            "[rules.\"no-action\"]\npriority = 100\napplies_to = [\"com.x\"]",
            1,
        )
        .unwrap();
        assert!(t.rules.is_empty());
        // 显式 suppress 保留（区分"未声明"与"声明 suppress"）
        let t2 = RuleTable::from_toml("[rules.\"ok\"]\naction = \"suppress\"", 1).unwrap();
        assert_eq!(t2.rules.len(), 1);
        assert_eq!(t2.rules[0].action, RuleAction::Suppress);
        // 混合：一条缺 action 剔除、一条合法保留
        let t3 = RuleTable::from_toml(
            "[rules.\"bad\"]\npriority = 10\n\n[rules.\"good\"]\naction = \"freeze\"",
            1,
        )
        .unwrap();
        assert_eq!(t3.rules.len(), 1);
        assert_eq!(t3.rules[0].id, "good");
    }

    #[test]
    fn match_condition_and_source() {
        let src = r#"
[rules."wake-bcast"]
applies_to = [ "com.tencent.mobileqq" ]
condition = "wakeup"
source = "broadcast"
action = "suppress"

[rules."wake-any"]
applies_to = [ "com.tencent.mobileqq" ]
condition = "wakeup"
action = "suppress"
"#;
        let t = RuleTable::from_toml(src, 1).unwrap();
        // wakeup + broadcast → wake-bcast（同 priority=0，按定义顺序先命中）
        let h = t.peek("com.tencent.mobileqq", RuleCondition::Wakeup, Some("broadcast"), now()).unwrap();
        assert_eq!(h.id, "wake-bcast");
        // wakeup + service → wake-any（bcast 源限定不命中）
        let h2 = t.peek("com.tencent.mobileqq", RuleCondition::Wakeup, Some("service"), now()).unwrap();
        assert_eq!(h2.id, "wake-any");
        // leave 条件不命中 wakeup 规则（condition=wakeup ≠ leave）
        assert!(t.peek("com.tencent.mobileqq", RuleCondition::Leave, None, now()).is_none());
        // always 条件在任意事件命中
        let t2 = RuleTable::from_toml("[rules.\"a\"]\naction = \"exempt\"", 1).unwrap();
        assert!(t2.peek("x", RuleCondition::Leave, None, now()).is_some());
        assert!(t2.peek("x", RuleCondition::Wakeup, None, now()).is_some());
        assert!(t2.peek("x", RuleCondition::Focus, None, now()).is_some());
        assert!(t2.peek("x", RuleCondition::Always, None, now()).is_some());
    }

    #[test]
    fn match_priority() {
        let src = r#"
[rules."low"]
applies_to = [ "com.example.app" ]
action = "exempt"

[rules."high"]
applies_to = [ "com.example.app" ]
action = "freeze"
priority = 50
"#;
        let t = RuleTable::from_toml(src, 1).unwrap();
        let h = t.peek("com.example.app", RuleCondition::Leave, None, now()).unwrap();
        assert_eq!(h.id, "high"); // priority 50 > 0
        assert_eq!(h.action, RuleAction::Freeze);
    }

    #[test]
    fn match_throttle() {
        let src = r#"
[rules."t"]
applies_to = [ "*" ]
action = "suppress"
throttle = 60
"#;
        let mut t = RuleTable::from_toml(src, 1).unwrap();
        let t0 = Instant::now();
        // 首次命中
        assert!(t.hit("com.example.app", RuleCondition::Wakeup, None, t0).is_some());
        // 窗口内不命中
        assert!(t.hit("com.example.app", RuleCondition::Wakeup, None, t0 + Duration::from_secs(30)).is_none());
        // 窗口外命中
        assert!(t.hit("com.example.app", RuleCondition::Wakeup, None, t0 + Duration::from_secs(61)).is_some());
        // 其他包不受节流影响
        assert!(t.hit("com.other.app", RuleCondition::Wakeup, None, t0 + Duration::from_secs(30)).is_some());
        // 无 throttle → 恒命中
        let t2 = RuleTable::from_toml("[rules.\"x\"]\naction = \"suppress\"", 2).unwrap();
        assert!(t2.peek("p", RuleCondition::Wakeup, None, t0).is_some());
        assert!(t2.peek("p", RuleCondition::Wakeup, None, t0 + Duration::from_secs(1)).is_some());
    }

    #[test]
    fn match_expires() {
        let src = r#"
[rules."e"]
applies_to = [ "com.example.app" ]
action = "suppress"
expires_at = "2000-01-01"
"#;
        let t = RuleTable::from_toml(src, 1).unwrap();
        // 过期（2000 年远早于今天）→ 不命中
        assert!(t.peek("com.example.app", RuleCondition::Wakeup, None, now()).is_none());
        // 无过期 → 命中
        let t2 = RuleTable::from_toml("[rules.\"x\"]\naction = \"suppress\"", 2).unwrap();
        assert!(t2.peek("p", RuleCondition::Wakeup, None, now()).is_some());
    }

    #[test]
    fn parse_date_ok_and_bad() {
        assert_eq!(parse_date("2026-12-31"), Some((2026, 12, 31)));
        assert_eq!(parse_date("2026-1-5"), Some((2026, 1, 5)));
        assert_eq!(parse_date("2026-13-01"), None);
        assert_eq!(parse_date("2026-00-01"), None);
        assert_eq!(parse_date("2026-01-32"), None);
        assert_eq!(parse_date("2026/01/01"), None);
        assert_eq!(parse_date(""), None);
        assert_eq!(parse_date("abc"), None);
    }

    #[test]
    fn hit_vs_peek_consistency() {
        let src = "[rules.\"x\"]\napplies_to = [\"com.example.app\"]\naction = \"freeze\"";
        let mut t = RuleTable::from_toml(src, 1).unwrap();
        let t0 = Instant::now();
        let h1 = t.hit("com.example.app", RuleCondition::Leave, None, t0).unwrap();
        assert_eq!(h1.action, RuleAction::Freeze);
        let p1 = t.peek("com.example.app", RuleCondition::Leave, None, t0).unwrap();
        assert_eq!(p1.action, RuleAction::Freeze);
        assert_eq!(p1.id, h1.id);
        // peek 不更新节流状态（无 throttle 规则，此处验证 hit 返回快照一致）
        assert!(t.hit("com.example.app", RuleCondition::Leave, None, t0 + Duration::from_secs(1)).is_some());
    }

    #[test]
    fn action_parse() {
        assert_eq!(RuleAction::parse("suppress"), Some(RuleAction::Suppress));
        assert_eq!(RuleAction::parse("exempt"), Some(RuleAction::Exempt));
        assert_eq!(RuleAction::parse("freeze"), Some(RuleAction::Freeze));
        assert_eq!(RuleAction::parse("discard"), Some(RuleAction::Discard));
        assert_eq!(RuleAction::parse("nuke"), None);
        assert_eq!(RuleAction::Suppress.as_str(), "suppress");
        assert_eq!(RuleCondition::parse("always"), Some(RuleCondition::Always));
        assert_eq!(RuleCondition::parse("leave"), Some(RuleCondition::Leave));
        assert_eq!(RuleCondition::parse("wakeup"), Some(RuleCondition::Wakeup));
        assert_eq!(RuleCondition::parse("focus"), Some(RuleCondition::Focus));
        assert_eq!(RuleCondition::parse("turbo"), None);
    }
}
