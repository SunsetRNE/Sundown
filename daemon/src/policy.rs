//! L3 策略模型：conf/policy.toml 的解析与校验（docs/l3-plan.md §0.2/§0.3）。
//!
//! 铁律：解析失败 → 调用方保留旧策略表（失败安全）。未知键/段 → 警告不致命（前向兼容）。

use crate::toml::{parse, TomlEntry, TomlValue};
use crate::{logw, paths};

/// 策略默认值（policy.toml 缺失/解析失败时的兜底：策略关闭，观测优先）
#[derive(Debug, Clone)]
pub struct Policy {
    /// 总开关（false = 只观测，不冻结）
    pub enabled: bool,
    /// 退后台 grace 秒数（防抖动）
    pub grace_seconds: u64,
    /// 解冻后冷却秒数（防"解冻-立即再冻"抖动）
    pub cooldown_seconds: u64,
    /// 强制冻结名单（命中即冻，优先级高于豁免动作，但白名单仍优先）
    pub force: Vec<String>,
    /// 永不冻结白名单
    pub whitelist: Vec<String>,
    /// 豁免动作：前台服务持有者不冻（dex 侧判定字段 fg=1）
    pub keep_fg_service: bool,
    /// 豁免动作：媒体播放持有者不冻（dex 侧判定字段 media=1）
    pub keep_media: bool,
    /// 防御 hook 组（L3 仅解析+展示，不启用）
    pub defense_anr: bool,
    pub defense_cached_optimizer: bool,
    /// 策略文件修订号（mtime 秒；热加载识别用）
    pub revision: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            enabled: false,
            grace_seconds: 30,
            cooldown_seconds: 60,
            force: Vec::new(),
            whitelist: Vec::new(),
            keep_fg_service: true,
            keep_media: true,
            defense_anr: false,
            defense_cached_optimizer: false,
            revision: 0,
        }
    }
}

impl Policy {
    /// 从 TOML 文本构建策略；Err = 解析失败（调用方保留旧表）
    pub fn from_toml(src: &str, revision: u64) -> Result<Policy, String> {
        let entries = parse(src).map_err(|(lineno, msg)| format!("第 {} 行: {}", lineno, msg))?;
        let mut p = Policy::default();
        p.revision = revision;
        for e in &entries {
            apply_entry(&mut p, e);
        }
        // 简单校验：grace/cooldown 非负已由 Int 类型保证（负数允许但无意义，钳 0）
        Ok(p)
    }

    /// 从磁盘读取策略文件；失败返回 None（缺失 = 未配置，用默认）
    pub fn load() -> Option<(Policy, String)> {
        let text = std::fs::read_to_string(paths::POLICY_FILE).ok()?;
        let revision = std::fs::metadata(paths::POLICY_FILE)
            .ok()
            .and_then(|m| m.modified().ok())
            .map(|t| t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0))
            .unwrap_or(0);
        match Policy::from_toml(&text, revision) {
            Ok(p) => Some((p, text)),
            Err(e) => {
                logw!("策略解析失败（保留旧表）: {} —— {}", paths::POLICY_FILE, e);
                None
            }
        }
    }

    /// 白名单判定
    pub fn is_whitelisted(&self, pkg: &str) -> bool {
        self.whitelist.iter().any(|w| w == pkg)
    }

    /// 强制冻结名单判定
    pub fn is_forced(&self, pkg: &str) -> bool {
        self.force.iter().any(|f| f == pkg)
    }
}

fn apply_entry(p: &mut Policy, e: &TomlEntry) {
    let section = e.table.join(".");
    let val = &e.value;
    match (section.as_str(), e.key.as_str()) {
        ("general", "enabled") => p.enabled = bool_of(val, e, false),
        ("general", "grace_seconds") => p.grace_seconds = int_of(val, e, 30).max(0) as u64,
        ("general", "cooldown_seconds") => p.cooldown_seconds = int_of(val, e, 60).max(0) as u64,
        ("freeze", "force") => p.force = str_array_of(val, e),
        ("whitelist", "packages") => p.whitelist = str_array_of(val, e),
        ("whitelist", "keep_fg_service") => p.keep_fg_service = bool_of(val, e, true),
        ("whitelist", "keep_media") => p.keep_media = bool_of(val, e, true),
        ("defense", "anr_protect") => p.defense_anr = bool_of(val, e, false),
        ("defense", "cached_app_optimizer") => p.defense_cached_optimizer = bool_of(val, e, false),
        (s, k) => {
            if !s.is_empty() {
                logw!("策略未知键（忽略）: [{}] {} = {}", s, k, debug_val(val));
            } else {
                logw!("策略未知顶层键（忽略）: {} = {}", k, debug_val(val));
            }
        }
    }
}

fn bool_of(v: &TomlValue, e: &TomlEntry, def: bool) -> bool {
    match v {
        TomlValue::Bool(b) => *b,
        _ => {
            logw!("策略键类型错误（用默认 {}）: {} = {}", def, e.key, debug_val(v));
            def
        }
    }
}

fn int_of(v: &TomlValue, e: &TomlEntry, def: i64) -> i64 {
    match v {
        TomlValue::Int(n) => *n,
        _ => {
            logw!("策略键类型错误（用默认 {}）: {} = {}", def, e.key, debug_val(v));
            def
        }
    }
}

fn str_array_of(v: &TomlValue, e: &TomlEntry) -> Vec<String> {
    match v {
        TomlValue::StrArray(items) => items.clone(),
        _ => {
            logw!("策略键类型错误（用空数组）: {} = {}", e.key, debug_val(v));
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

    #[test]
    fn parse_policy_ok() {
        let src = r#"
[general]
enabled = true
grace_seconds = 15
cooldown_seconds = 60

[freeze]
force = [ "com.greedy.app" ]

[whitelist]
packages = [ "com.android.settings" ]
keep_fg_service = true
keep_media = false

[defense]
anr_protect = true
"#;
        let p = Policy::from_toml(src, 42).unwrap();
        assert!(p.enabled);
        assert_eq!(p.grace_seconds, 15);
        assert_eq!(p.cooldown_seconds, 60);
        assert_eq!(p.force, vec!["com.greedy.app"]);
        assert!(p.is_whitelisted("com.android.settings"));
        assert!(!p.is_whitelisted("x.y"));
        assert!(p.is_forced("com.greedy.app"));
        assert!(p.keep_fg_service);
        assert!(!p.keep_media);
        assert!(p.defense_anr);
        assert!(!p.defense_cached_optimizer);
        assert_eq!(p.revision, 42);
    }

    #[test]
    fn parse_policy_bad() {
        assert!(Policy::from_toml("k = ", 0).is_err());
        assert!(Policy::from_toml("[general\nenabled = true", 0).is_err());
    }

    #[test]
    fn unknown_key_ignored() {
        let p = Policy::from_toml("[general]\nenabled = true\nfuture_key = 1", 1).unwrap();
        assert!(p.enabled);
    }
}