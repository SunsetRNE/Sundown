//! C2（v0.8-l3）分析建议（只建议不执行）
//!
//! 数据源 = C1 使用画像快照（profile.rs 纯内存聚合），零新增采集。
//! 输出 = 建议列表（JSON），sunctl analyze 导出；WebUI 画像页消费。
//!
//! 铁律：
//! - **只建议不自动执行**（失败安全哲学）：建议仅文本 + 动作提示
//!   （写 rules.toml / policy.toml 的手工指引），daemon 绝不自动改配置
//! - **本地分析不走云端**（已定决策）：纯本地计算，无网络面
//! - 保守可解释：每条建议带 level/kind/依据（detail），阈值用相对量
//!   （占比/速率）而非硬编码绝对值，数据不足明确提示继续采集
//! - 纯函数（输入画像快照 → 建议列表），可单测

use crate::profile::ProfileTable;
use std::collections::HashMap;

/// 分析建议（单条）
#[derive(Debug, Clone)]
pub struct Suggestion {
    /// info（观察）/ warn（关注）/ critical（强烈建议）
    pub level: &'static str,
    /// 建议类别（wakeup_storm / source_pattern / jitter / throttle / exempt / data_insufficient）
    pub kind: &'static str,
    /// 目标包名（全局建议为 None）
    pub pkg: Option<String>,
    /// 一句话标题
    pub title: String,
    /// 依据（数据事实，可审计）
    pub detail: String,
    /// 动作提示（手工执行指引，如写 rules.toml 的完整规则段）
    pub action_hint: String,
}

/// 分析画像表 → 建议列表（纯函数）
pub fn analyze(profile: &ProfileTable) -> Vec<Suggestion> {
    let mut out: Vec<Suggestion> = Vec::new();
    let app_count = profile.app_count();
    let total_wakeups = profile.total_wakeups();

    // ---- 数据充分性门槛：不足则明确提示继续采集（不输出噪音建议） ----
    if app_count < 3 || total_wakeups < 10 {
        out.push(Suggestion {
            level: "info",
            kind: "data_insufficient",
            pkg: None,
            title: "画像数据不足，建议继续采集".to_string(),
            detail: format!(
                "当前 {} 个 app / {} 次唤醒；分析门槛 = ≥3 app 且 ≥10 次唤醒（避免小样本误判）",
                app_count, total_wakeups
            ),
            action_hint: "正常使用几天后重跑 sunctl analyze".to_string(),
        });
        return out;
    }

    // ---- 1. 疯狂唤醒者识别（占比过半 或 绝对 ≥10 次） ----
    let half = (total_wakeups as f64 / 2.0).ceil() as u64;
    for (pkg, p) in profile.top_wakeups(app_count) {
        if p.wakeup_count >= 10 && p.wakeup_count >= half {
            let rate = wakeup_rate_per_hour(p);
            out.push(Suggestion {
                level: "warn",
                kind: "wakeup_storm",
                pkg: Some(pkg.clone()),
                title: format!("{} 疑似唤醒风暴（{} 次）", pkg, p.wakeup_count),
                detail: format!(
                    "占全部唤醒 {:.0}%（{} / {}）{}",
                    p.wakeup_count as f64 * 100.0 / total_wakeups as f64,
                    p.wakeup_count,
                    total_wakeups,
                    rate.map(|r| format!("，约 {:.1} 次/小时", r)).unwrap_or_default()
                ),
                action_hint: format!(
                    "写 rules.toml 抑制唤醒（不重编译 dex）：\n[rules.\"{}-storm\"]\napplies_to = [ \"{}\" ]\ncondition = \"wakeup\"\naction = \"suppress\"\nthrottle = 60",
                    pkg.replace('.', "-"),
                    pkg
                ),
            });
        }
    }

    // ---- 2. 唤醒源模式聚类（单源占比 >70%） ----
    for (pkg, p) in profile.top_wakeups(app_count) {
        if p.wakeup_count < 5 {
            continue;
        }
        if let Some((src, n)) = dominant_source(&p.wakeup_sources, p.wakeup_count) {
            if n as f64 >= p.wakeup_count as f64 * 0.7 {
                out.push(Suggestion {
                    level: "info",
                    kind: "source_pattern",
                    pkg: Some(pkg.clone()),
                    title: format!("{} 唤醒以 {} 为主（{:.0}%）", pkg, src, n as f64 * 100.0 / p.wakeup_count as f64),
                    detail: format!(
                        "{} 源 {} 次 / 共 {} 次；模式=广播风暴/服务拉起/任务闹钟",
                        src, n, p.wakeup_count
                    ),
                    action_hint: match src.as_str() {
                        "broadcast" => "若为骚扰广播：rules.toml 加 source=\"broadcast\" 的 suppress 规则，或 policy.toml 配 receiver_gate 白名单".to_string(),
                        "service" => "若为后台服务拉起：rules.toml 加 source=\"service\" 的 suppress 规则，或 policy.toml 配 service_gate".to_string(),
                        "pendingintent" => "若为闹钟/任务：评估保留（用户主动）或 rules.toml 加 source=\"pendingintent\" suppress".to_string(),
                        _ => "观察该源的真实业务含义后再决定".to_string(),
                    },
                });
            }
        }
    }

    // ---- 3. 抖动检测（频繁冻结又频繁唤醒 → 解冻-再冻抖动） ----
    for (pkg, p) in profile.top_wakeups(app_count) {
        if p.freeze_count >= 2 && p.wakeup_count >= p.freeze_count * 3 {
            out.push(Suggestion {
                level: "warn",
                kind: "jitter",
                pkg: Some(pkg.clone()),
                title: format!("{} 疑似冻结-唤醒抖动（冻 {} / 醒 {}）", pkg, p.freeze_count, p.wakeup_count),
                detail: format!(
                    "冻结 {} 次、唤醒 {} 次（唤醒 ≥ 冻结×3）：反复冻结-解冻消耗资源，用户体验差",
                    p.freeze_count, p.wakeup_count
                ),
                action_hint: format!(
                    "二选一：① 豁免（rules.toml action=\"exempt\"）；② 加大 grace（policy.toml [general] grace_seconds，当前观察模式可先不处理）"
                ),
            });
        }
    }

    // ---- 4. 频繁使用却冻结（解冻卡顿风险 → 豁免建议） ----
    for (pkg, p) in profile.top_wakeups(app_count) {
        if p.focus_count >= 3 && p.freeze_count > 0 {
            out.push(Suggestion {
                level: "info",
                kind: "exempt",
                pkg: Some(pkg.clone()),
                title: format!("{} 频繁使用但曾被冻结（前台 {} 次 / 冻结 {} 次）", pkg, p.focus_count, p.freeze_count),
                detail: format!(
                    "前台 {} 次（累计 {:.1}s）、冻结 {} 次：用户常用 app 被冻结有解冻卡顿风险",
                    p.focus_count,
                    p.focus_ms as f64 / 1000.0,
                    p.freeze_count
                ),
                action_hint: format!(
                    "rules.toml 豁免：\n[rules.\"{}-keep\"]\naction = \"exempt\"\napplies_to = [ \"{}\" ]\npriority = 200",
                    pkg.replace('.', "-"),
                    pkg
                ),
            });
        }
    }

    // ---- 5. 节流参数建议（中频唤醒但未达风暴 → 适度节流） ----
    let already_warned: Vec<String> = out
        .iter()
        .filter(|s| s.kind == "wakeup_storm")
        .filter_map(|s| s.pkg.clone())
        .collect();
    for (pkg, p) in profile.top_wakeups(app_count) {
        if p.wakeup_count >= 5 && p.wakeup_count < 10 && !already_warned.contains(&pkg) {
            out.push(Suggestion {
                level: "info",
                kind: "throttle",
                pkg: Some(pkg.clone()),
                title: format!("{} 唤醒偏多（{} 次），可适度节流", pkg, p.wakeup_count),
                detail: format!("{} 次唤醒未达风暴阈值，但高于均值；节流可降低后台唤醒频率", p.wakeup_count),
                action_hint: "policy.toml [general] wake_throttle_seconds 调大（如 60→120），或 rules.toml 加 throttle=60 的 suppress 规则".to_string(),
            });
        }
    }

    out
}

/// 主导源（占比最高且 >=2 次的源）
fn dominant_source(sources: &HashMap<String, u64>, total: u64) -> Option<(String, u64)> {
    let mut best: Option<(String, u64)> = None;
    for (k, v) in sources {
        if *v < 2 {
            continue;
        }
        if best.as_ref().map_or(true, |(_, bv)| v > bv) {
            best = Some((k.clone(), *v));
        }
    }
    let _ = total;
    best
}

/// 每小时唤醒率（时间窗 = first_seen→last_seen；窗 <60s 或无法计算 → None）
fn wakeup_rate_per_hour(p: &crate::profile::AppProfile) -> Option<f64> {
    if p.last_seen_at <= p.first_seen_at || p.last_seen_at - p.first_seen_at < 60 {
        return None;
    }
    let hours = (p.last_seen_at - p.first_seen_at) as f64 / 3600.0;
    Some(p.wakeup_count as f64 / hours)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_profile() -> ProfileTable {
        let mut t = ProfileTable::default();
        // com.a：风暴（广播为主）
        for _ in 0..30 {
            t.on_wakeup("com.a", "broadcast");
        }
        // com.b：抖动（冻结多 + 唤醒多）
        for _ in 0..9 {
            t.on_wakeup("com.b", "service");
        }
        for _ in 0..3 {
            t.on_freeze("com.b");
        }
        // com.c：频繁使用被冻结
        for _ in 0..4 {
            t.on_focus("com.c");
            t.on_leave("com.c");
        }
        t.on_freeze("com.c");
        // com.d：中频（5 次）
        for _ in 0..5 {
            t.on_wakeup("com.d", "pendingintent");
        }
        t
    }

    #[test]
    fn insufficient_data_returns_hint() {
        let mut t = ProfileTable::default();
        t.on_wakeup("com.a", "broadcast");
        t.on_wakeup("com.b", "broadcast");
        let out = analyze(&t);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, "data_insufficient");
    }

    #[test]
    fn storm_detected_with_hint() {
        let t = seed_profile();
        let out = analyze(&t);
        let storms: Vec<_> = out.iter().filter(|s| s.kind == "wakeup_storm").collect();
        assert_eq!(storms.len(), 1);
        assert_eq!(storms[0].pkg.as_deref(), Some("com.a"));
        // 动作提示含 rules.toml 指引
        assert!(storms[0].action_hint.contains("[rules."));
        assert!(storms[0].action_hint.contains("suppress"));
    }

    #[test]
    fn source_pattern_detected() {
        let t = seed_profile();
        let out = analyze(&t);
        let pats: Vec<_> = out.iter().filter(|s| s.kind == "source_pattern").collect();
        // com.a broadcast 100% → 命中
        assert!(pats.iter().any(|s| s.pkg.as_deref() == Some("com.a")));
        // com.b service 100% 且 >=5 次 → 命中
        assert!(pats.iter().any(|s| s.pkg.as_deref() == Some("com.b")));
    }

    #[test]
    fn jitter_and_exempt_detected() {
        let t = seed_profile();
        let out = analyze(&t);
        assert!(out.iter().any(|s| s.kind == "jitter" && s.pkg.as_deref() == Some("com.b")));
        assert!(out.iter().any(|s| s.kind == "exempt" && s.pkg.as_deref() == Some("com.c")));
    }

    #[test]
    fn throttle_for_mid_frequency() {
        let t = seed_profile();
        let out = analyze(&t);
        // com.d：5 次未达风暴 → throttle 建议（com.a 30 次风暴不重复建议）
        assert!(out.iter().any(|s| s.kind == "throttle" && s.pkg.as_deref() == Some("com.d")));
        let storm_pkgs: Vec<_> = out
            .iter()
            .filter(|s| s.kind == "wakeup_storm")
            .filter_map(|s| s.pkg.clone())
            .collect();
        assert!(!storm_pkgs.contains(&"com.d".to_string()));
    }

    #[test]
    fn rate_requires_window() {
        let t = seed_profile();
        // seed 画像时间窗 <60s → 速率无法计算（None），避免小窗口误导
        let p = t.get("com.a").unwrap();
        assert!(wakeup_rate_per_hour(p).is_none());
    }
}