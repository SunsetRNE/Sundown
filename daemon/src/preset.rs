//! L3 情景预设（conf/action.toml）：预设 = [general] 参数组。
//!
//! 参考 Cerberus action.toml 预设体系，落地为"内存切换"模型：
//!   - `policy preset apply <name>` → daemon 内存覆盖 [general] 参数（不动磁盘 policy.toml）
//!   - `policy preset clear`       → 重新加载磁盘 policy.toml，回落磁盘参数
//!   - 预设只覆盖 enabled / grace_seconds / cooldown_seconds / keep_fg_service / keep_media；
//!     白名单 / force / per-app 策略始终以磁盘 policy.toml 为准（预设不触碰）
//!
//! 失败安全：action.toml 缺失/解析失败 → 空预设表（预设功能不可用，不致命）；
//! 未知键 → 警告不致命（前向兼容）。

use std::collections::HashMap;

use crate::toml::{parse, TomlEntry, TomlValue};
use crate::{logw, paths};

/// 单个预设：对 [general] 的覆盖参数
#[derive(Debug, Clone)]
pub struct Preset {
    pub enabled: bool,
    pub grace_seconds: u64,
    pub cooldown_seconds: u64,
    pub keep_fg_service: bool,
    pub keep_media: bool,
}

/// 预设表（action.toml 解析结果；空表 = 无预设可用）
#[derive(Debug, Clone, Default)]
pub struct PresetTable {
    pub presets: HashMap<String, Preset>,
    /// action.toml 修订号（mtime 秒）
    pub revision: u64,
}

impl PresetTable {
    /// 从 TOML 文本构建预设表；Err = 解析失败（调用方保留空表/旧表）
    pub fn from_toml(src: &str, revision: u64) -> Result<PresetTable, String> {
        let entries = parse(src).map_err(|(lineno, msg)| format!("第 {} 行: {}", lineno, msg))?;
        let mut t = PresetTable { presets: HashMap::new(), revision };
        for e in &entries {
            let section = e.table.join(".");
            if let Some(name) = section.strip_prefix("presets.") {
                if name.is_empty() {
                    logw!("预设 [presets] 裸段（缺名称）忽略: {} = {}", e.key, debug_val(&e.value));
                    continue;
                }
                let p = t.presets.entry(name.to_string()).or_insert_with(|| Preset {
                    enabled: false,
                    grace_seconds: 30,
                    cooldown_seconds: 60,
                    keep_fg_service: true,
                    keep_media: true,
                });
                let val = &e.value;
                match e.key.as_str() {
                    "enabled" => p.enabled = bool_of(val, e),
                    "grace_seconds" => p.grace_seconds = int_of(val, e).max(0) as u64,
                    "cooldown_seconds" => p.cooldown_seconds = int_of(val, e).max(0) as u64,
                    "keep_fg_service" => p.keep_fg_service = bool_of(val, e),
                    "keep_media" => p.keep_media = bool_of(val, e),
                    k => logw!("预设 [presets.{}] 未知键（忽略）: {} = {}", name, k, debug_val(val)),
                }
            } else if !section.is_empty() {
                logw!("预设未知段（忽略）: [{}] {} = {}", section, e.key, debug_val(&e.value));
            }
        }
        Ok(t)
    }

    /// 从磁盘加载 action.toml；缺失返回空表（预设功能不可用，不致命）
    pub fn load() -> PresetTable {
        let Ok(text) = std::fs::read_to_string(paths::ACTION_FILE) else {
            return PresetTable::default();
        };
        let revision = std::fs::metadata(paths::ACTION_FILE)
            .ok()
            .and_then(|m| m.modified().ok())
            .map(|t| t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0))
            .unwrap_or(0);
        match PresetTable::from_toml(&text, revision) {
            Ok(t) => t,
            Err(e) => {
                logw!("预设解析失败（空表）: {} —— {}", paths::ACTION_FILE, e);
                PresetTable::default()
            }
        }
    }

    /// 预设名列表（稳定排序，供 list 输出）
    pub fn names(&self) -> Vec<String> {
        let mut v: Vec<String> = self.presets.keys().cloned().collect();
        v.sort();
        v
    }
}

fn bool_of(v: &TomlValue, e: &TomlEntry) -> bool {
    match v {
        TomlValue::Bool(b) => *b,
        _ => {
            logw!("预设键类型错误（用 false）: {} = {}", e.key, debug_val(v));
            false
        }
    }
}

fn int_of(v: &TomlValue, e: &TomlEntry) -> i64 {
    match v {
        TomlValue::Int(n) => *n,
        _ => {
            logw!("预设键类型错误（用 0）: {} = {}", e.key, debug_val(v));
            0
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
    fn parse_presets_ok() {
        let src = r#"
[presets."watch"]
enabled = false
grace_seconds = 30
cooldown_seconds = 60

[presets."aggressive"]
enabled = true
grace_seconds = 10
cooldown_seconds = 60
keep_media = false
"#;
        let t = PresetTable::from_toml(src, 42).unwrap();
        assert_eq!(t.presets.len(), 2);
        let w = &t.presets["watch"];
        assert!(!w.enabled);
        assert_eq!(w.grace_seconds, 30);
        assert_eq!(w.cooldown_seconds, 60);
        assert!(w.keep_media); // 缺省 true
        let a = &t.presets["aggressive"];
        assert!(a.enabled);
        assert_eq!(a.grace_seconds, 10);
        assert!(!a.keep_media);
        assert_eq!(t.revision, 42);
        assert_eq!(t.names(), vec!["aggressive", "watch"]);
    }

    #[test]
    fn parse_presets_instant_v06() {
        // 立即墓碑档位（A2）：grace_seconds=0 必须解析为 0（int_of.max(0) 不钳 0）
        let src = r#"
[presets."instant"]
enabled = true
grace_seconds = 0
cooldown_seconds = 120
keep_fg_service = true
keep_media = true
"#;
        let t = PresetTable::from_toml(src, 7).unwrap();
        let i = &t.presets["instant"];
        assert!(i.enabled);
        assert_eq!(i.grace_seconds, 0);
        assert_eq!(i.cooldown_seconds, 120);
        assert!(i.keep_fg_service);
        assert!(i.keep_media);
        // 负数钳 0（与 max(0) 一致）
        let src2 = "[presets.\"x\"]\ngrace_seconds = -5\n";
        let t2 = PresetTable::from_toml(src2, 1).unwrap();
        assert_eq!(t2.presets["x"].grace_seconds, 0);
    }

    #[test]
    fn parse_presets_bad() {
        assert!(PresetTable::from_toml("k = ", 0).is_err());
        // 未知键忽略不致命
        let t = PresetTable::from_toml("[presets.\"x\"]\nenabled = true\nfuture = 1", 1).unwrap();
        assert!(t.presets["x"].enabled);
    }

    #[test]
    fn parse_presets_unknown_section() {
        // 非 presets 段忽略
        let t = PresetTable::from_toml("[other]\nfoo = 1", 1).unwrap();
        assert!(t.presets.is_empty());
    }
}
