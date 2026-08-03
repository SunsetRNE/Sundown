//! L3 策略引擎：事件消费 + 决策状态机（docs/l3-plan.md §0.3/§0.4）。
//!
//! 状态机：
//!   focus pkg=P ──► P 冻结中 → 解冻 + 冷却窗口；旧前台 Q 离开 → 豁免判定 →
//!                   whitelist/豁免动作/冷却 → 跳过；force → 立即冻结；否则 grace 计时
//!   tick ──► grace 到期且仍非前台 → 冻结（uid 级）；策略关闭 → 全量解冻
//!   wakeup pkg=P ──► P 冻结中 → 解冻 + 冷却（防唤醒失效）
//!   force-stop pkg=P ──► 清冻结/计时/索引
//!
//! 失败安全：冻结写失败只留痕不崩溃；策略解析失败保留旧表（policy.rs）。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::events::{EvAction, EvLevel, EventBuffer};
use crate::freezer;
use crate::network::{NetSampler, DEFAULT_THRESHOLD, DEFAULT_WINDOW};
use crate::policy::{AppMode, Policy, PushMode};
use crate::preset::{Preset, PresetTable};
use crate::{logi, logw};

/// 每个包最近一次 focus 事件的豁免判定字段
#[derive(Debug, Clone, Copy)]
pub struct ExemptFlags {
    pub fg_service: bool,
    pub media: bool,
    /// 定位活动（dex 侧 AppOps 判定 loc=1，v0.4.20-l3）
    pub location: bool,
}

/// 策略引擎状态（由 DaemonState.engine 持有，调用方持锁操作）
pub struct EngineState {
    pub policy: Policy,
    /// pkg → 冻结时刻（当前冻结表）
    pub frozen: HashMap<String, Instant>,
    /// pkg → (grace 开始时刻, 该包 grace 秒数)——per-app 各自时长（strict 8s / 覆盖值）
    pub grace: HashMap<String, (Instant, u64)>,
    /// pkg → 冷却截止时刻（解冻后免冻窗口）
    pub cooldown: HashMap<String, Instant>,
    /// pkg → 最近一次豁免判定（focus 事件携带）
    pub exempt: HashMap<String, ExemptFlags>,
    /// 当前前台（恒不冻结）
    pub last_focus: Option<String>,
    /// pkg → 最近一次 proc-add 携带的 uid（包表兜底补充）
    pub pkg_uids: HashMap<String, u32>,
    /// pkg → 存活 pid 集合（proc-add/remove 维护；force-stop/进程核验用）
    pub pkg_pids: HashMap<String, Vec<u32>>,
    /// 结构化事件缓冲（日志数据层；环形 256，覆盖最旧——观测可损失）
    pub events: EventBuffer,
    /// 情景预设表（conf/action.toml；reload 时一并刷新，空表 = 预设不可用）
    pub presets: PresetTable,
    /// 当前生效预设名（None = 磁盘 policy.toml 参数）
    pub active_preset: Option<String>,
    /// 高网络负载采样器（keep_high_network 豁免判定；uid → 流量基线）
    pub net: NetSampler,
    pub freeze_ops: u64,
    pub unfreeze_ops: u64,
    pub wakeup_thaws: u64,
    /// tick 计数（v0.4.22-l3：低频对账节拍用）
    pub tick_count: u64,
}

impl Default for EngineState {
    fn default() -> Self {
        Self {
            policy: Policy::default(),
            frozen: HashMap::new(),
            grace: HashMap::new(),
            cooldown: HashMap::new(),
            exempt: HashMap::new(),
            last_focus: None,
            pkg_uids: HashMap::new(),
            pkg_pids: HashMap::new(),
            events: EventBuffer::default(),
            presets: PresetTable::default(),
            active_preset: None,
            net: NetSampler::new(),
            freeze_ops: 0,
            unfreeze_ops: 0,
            wakeup_thaws: 0,
            tick_count: 0,
        }
    }
}

impl EngineState {
    // ---------------- 事件入口 ----------------

    /// event focus pkg=P [fg=0|1] [media=0|1]
    ///
    /// 注意：fg/media 参数来自 focus 事件行（dex hook 回调侧仅登记 pkg，未携带
    /// 实时判定），**不可用于覆盖 exempt 表**——否则会把 ExemptMonitor 独立线程
    /// 上报的正确豁免判定（fg=true 前台服务/媒体）冲掉，导致退后台即有前台服务的
    /// app 被误计时/误冻（2026-08-02 真机实证：微信 fg=true 被 focus 事件覆盖后
    /// 进入 grace）。exempt 表只由 on_exempt 维护。
    pub fn on_focus(&mut self, pkg: &str, _fg: bool, _media: bool) {
        let now = Instant::now();

        // 新前台冻结中 → 解冻 + 冷却
        let was_frozen = self.frozen.remove(pkg).is_some();
        if was_frozen {
            if freezer::unfreeze_pkg(pkg) {
                self.unfreeze_ops += 1;
                logi!("L3 前台解冻: {}（解冻累计 {}）", pkg, self.unfreeze_ops);
                self.events.push_app(
                    EvLevel::Event,
                    EvAction::Unfreeze,
                    pkg,
                    Some("foreground"),
                    None,
                );
            } else {
                logw!("L3 前台解冻失败: {}", pkg);
                self.events.push_app(
                    EvLevel::Warn,
                    EvAction::Unfreeze,
                    pkg,
                    Some("foreground_failed"),
                    None,
                );
            }
            self.cooldown.insert(pkg.to_string(), now + self.cooldown_dur());
        } else {
            // v0.4.22-l3 兜底：frozen 表无记录但 uid 实际冻结（daemon 重启残留 /
            // 事件丢失导致的表状态失真）→ 仍解冻，防"能打开但点击无响应"（ANR）
            if let Some(uid) = freezer::pkg_uid(pkg) {
                if freezer::uid_has_frozen_procs(uid) && freezer::unfreeze_pkg(pkg) {
                    self.unfreeze_ops += 1;
                    logw!("L3 前台兜底解冻（残留冻结）: {}", pkg);
                    self.events.push_app(
                        EvLevel::Warn,
                        EvAction::Unfreeze,
                        pkg,
                        Some("residual_thaw"),
                        None,
                    );
                    self.cooldown.insert(pkg.to_string(), now + self.cooldown_dur());
                }
            }
        }
        // 新前台取消其 grace（切回来了）
        self.grace.remove(pkg);

        // 旧前台离开决策
        if let Some(prev) = self.last_focus.clone() {
            if prev != pkg {
                self.decide_leave(&prev, now);
            }
        }
        self.last_focus = Some(pkg.to_string());
    }

    /// event wakeup pkg=P reason=...
    /// v0.4.19-l3：per-app keep_wakeup=false 时忽略唤醒（不解冻不取消 grace——
    /// FCM/交互唤醒风暴 app 保持冻结；事件留痕 reason=wakeup_ignored）。
    pub fn on_wakeup(&mut self, pkg: &str) {
        if !self.keep_wakeup(pkg) {
            logi!("L3 唤醒忽略（keep_wakeup=false）: {}", pkg);
            self.events.push_app(
                EvLevel::Info,
                EvAction::Exempt,
                pkg,
                Some("wakeup_ignored"),
                None,
            );
            return;
        }
        let now = Instant::now();
        if self.frozen.remove(pkg).is_some() {
            if freezer::unfreeze_pkg(pkg) {
                self.unfreeze_ops += 1;
                self.wakeup_thaws += 1;
                logi!("L3 唤醒解冻: {}（累计 {} 次）", pkg, self.wakeup_thaws);
                self.events.push_app(
                    EvLevel::Event,
                    EvAction::Unfreeze,
                    pkg,
                    Some("wakeup"),
                    None,
                );
            } else {
                logw!("L3 唤醒解冻失败: {}", pkg);
                self.events.push_app(
                    EvLevel::Warn,
                    EvAction::Unfreeze,
                    pkg,
                    Some("wakeup_failed"),
                    None,
                );
            }
            self.cooldown.insert(pkg.to_string(), now + self.cooldown_dur());
        }
        // 有唤醒说明进程存活且活跃，取消 pending grace
        self.grace.remove(pkg);
    }

    /// event exempt pkg=P fg=0|1 media=0|1 loc=0|1（dex 豁免判定监视器上行，独立线程 2s 节拍；
    /// dex 侧仅在判定值变化时上报 → 事件频率低，可直接入缓冲）
    /// v0.4.20-l3：新增 loc（定位 AppOps 判定）；旧 dex 不携带 loc 字段 → 缺省 false
    pub fn on_exempt(&mut self, pkg: &str, fg: bool, media: bool, loc: bool) {
        self.exempt.insert(
            pkg.to_string(),
            ExemptFlags {
                fg_service: fg,
                media,
                location: loc,
            },
        );
        let reason = if fg {
            "fg_service"
        } else if media {
            "media"
        } else if loc {
            "location"
        } else {
            "none"
        };
        self.events.push_app(
            EvLevel::Info,
            EvAction::Exempt,
            pkg,
            Some(reason),
            None,
        );
    }

    /// event proc-add pid=N pkg=P [uid=N]
    pub fn on_proc_add(&mut self, pid: u32, pkg: &str, uid: Option<u32>) {
        if let Some(u) = uid {
            self.pkg_uids.insert(pkg.to_string(), u);
        }
        let list = self.pkg_pids.entry(pkg.to_string()).or_default();
        if !list.contains(&pid) {
            list.push(pid);
        }
    }

    /// event proc-remove pid=N
    pub fn on_proc_remove(&mut self, pid: u32) {
        for list in self.pkg_pids.values_mut() {
            if let Some(pos) = list.iter().position(|p| *p == pid) {
                list.remove(pos);
                break;
            }
        }
    }

    /// event force-stop pkg=P：清冻结/计时/进程索引 + 保险解冻
    pub fn on_force_stop(&mut self, pkg: &str) {
        self.frozen.remove(pkg);
        self.grace.remove(pkg);
        self.cooldown.remove(pkg);
        self.pkg_pids.remove(pkg);
        self.pkg_uids.remove(pkg);
        // 保险解冻（幂等写 0；进程已死写失败无害）
        let _ = freezer::unfreeze_pkg(pkg);
        logi!("L3 force-stop 清理: {}", pkg);
        self.events.push_app(
            EvLevel::Event,
            EvAction::Close,
            pkg,
            Some("force_stop"),
            None,
        );
    }

    /// 策略重载（config.rs reload 回调）：成功替换；失败保留旧表。
    /// 预设表随热加载一并刷新（action.toml 变更即时生效）；生效中预设仍存在则
    /// 重放覆盖，已删除则回落磁盘参数。
    pub fn reload_policy(&mut self) {
        // 情景预设表随热加载一并刷新（缺失/解析失败 → 空表，预设功能降级不致命）
        self.presets = PresetTable::load();
        if let Some((p, _)) = Policy::load() {
            logi!(
                "L3 策略已重载: enabled={} grace={}s cooldown={}s whitelist={} force={} apps={}（revision={}）",
                p.enabled,
                p.grace_seconds,
                p.cooldown_seconds,
                p.whitelist.len(),
                p.force.len(),
                p.apps.len(),
                p.revision
            );
            self.events.push_system(
                EvLevel::Report,
                EvAction::Policy,
                Some("reloaded"),
                Some(&format!("enabled={} apps={} rev={}", p.enabled, p.apps.len(), p.revision)),
            );
            self.policy = p;
            // 生效中预设：仍存在 → 重放内存覆盖；已删除 → 回落磁盘参数
            if let Some(name) = self.active_preset.clone() {
                let keep = self.presets.presets.get(&name).cloned();
                match keep {
                    Some(pp) => {
                        self.overlay_preset(&name, &pp);
                        logi!("L3 预设保持（热加载后重放）: {}", name);
                    }
                    None => {
                        self.active_preset = None;
                        logw!("L3 预设 {} 已不存在，回落磁盘参数", name);
                    }
                }
            }
            // v0.4.22-l3：策略热更新即时生效——新增白名单/VPN/豁免后，
            // 已冻结的包立即解冻（此前只影响未来决策，实机反馈"开了白名单仍冻死"）
            let thaw: Vec<String> = self
                .frozen
                .keys()
                .filter(|p| self.should_never_freeze(p))
                .cloned()
                .collect();
            for pkg in thaw {
                if freezer::unfreeze_pkg(&pkg) {
                    self.unfreeze_ops += 1;
                    logw!("L3 策略热更新解冻: {}", pkg);
                }
                self.frozen.remove(&pkg);
                self.grace.remove(&pkg);
                self.events.push_app(
                    EvLevel::Info,
                    EvAction::Unfreeze,
                    &pkg,
                    Some("policy_reload"),
                    None,
                );
            }
        } else {
            logw!("L3 策略重载失败（保留旧表 revision={}）", self.policy.revision);
            self.events.push_system(
                EvLevel::Error,
                EvAction::Policy,
                Some("reload_failed"),
                None,
            );
        }
    }

    // ---------------- 情景预设（policy preset 命令） ----------------

    /// 应用预设：内存覆盖 [general] 参数（不动磁盘 policy.toml）。
    /// 白名单 / force / per-app 始终以磁盘 policy.toml 为准（预设不触碰）。
    pub fn apply_preset(&mut self, name: &str) -> Result<(), String> {
        let p = match self.presets.presets.get(name) {
            Some(p) => p.clone(),
            None => {
                let avail = self.presets.names();
                return Err(if avail.is_empty() {
                    "预设表为空（action.toml 缺失或未定义预设）".to_string()
                } else {
                    format!("预设不存在: {}（可用: {}）", name, avail.join(", "))
                });
            }
        };
        self.overlay_preset(name, &p);
        self.active_preset = Some(name.to_string());
        logi!(
            "L3 预设应用: {}（enabled={} grace={}s cooldown={}s keep_fg={} keep_media={}）",
            name,
            p.enabled,
            p.grace_seconds,
            p.cooldown_seconds,
            p.keep_fg_service,
            p.keep_media
        );
        self.events.push_system(
            EvLevel::Report,
            EvAction::Policy,
            Some("preset"),
            Some(&format!("apply={} enabled={} grace={}s", name, p.enabled, p.grace_seconds)),
        );
        Ok(())
    }

    /// 清除预设：重新加载磁盘 policy.toml，回落磁盘参数。
    /// 磁盘不可读时保留现状（失败安全，与 reload_policy 一致）。
    pub fn clear_preset(&mut self) {
        let prev = self.active_preset.take();
        if let Some((p, _)) = Policy::load() {
            self.policy = p;
            if let Some(name) = &prev {
                logi!(
                    "L3 预设清除: {}（回落磁盘参数 revision={}）",
                    name,
                    self.policy.revision
                );
                self.events.push_system(
                    EvLevel::Report,
                    EvAction::Policy,
                    Some("preset"),
                    Some(&format!("clear={}", name)),
                );
            } else {
                logi!("L3 预设清除：当前无生效预设（重载磁盘参数）");
            }
        } else {
            self.active_preset = prev; // 恢复原状
            logw!("L3 预设清除失败（policy.toml 不可读，保留现状）");
        }
    }

    /// 参数覆盖（apply / reload 重放共用；不推事件）
    fn overlay_preset(&mut self, name: &str, p: &Preset) {
        self.policy.enabled = p.enabled;
        self.policy.grace_seconds = p.grace_seconds;
        self.policy.cooldown_seconds = p.cooldown_seconds;
        self.policy.keep_fg_service = p.keep_fg_service;
        self.policy.keep_media = p.keep_media;
        let _ = name;
    }

    // ---------------- 定时驱动（main 主循环 tick 调用） ----------------

    /// 周期性推进：grace 到期冻结 / 冷却清理 / 策略关闭全量解冻 / 进程核验
    pub fn tick(&mut self) {
        let now = Instant::now();
        if !self.policy.enabled {
            // 观测模式：全量解冻（幂等），清空状态
            if !self.frozen.is_empty() {
                let pkgs: Vec<String> = self.frozen.keys().cloned().collect();
                for p in &pkgs {
                    if freezer::unfreeze_pkg(p) {
                        self.unfreeze_ops += 1;
                    }
                }
                logi!("L3 策略关闭，解冻 {} 个包", pkgs.len());
                self.events.push_system(
                    EvLevel::Info,
                    EvAction::Unfreeze,
                    Some("policy_disabled"),
                    Some(&format!("unfroze {} pkgs", pkgs.len())),
                );
            }
            self.frozen.clear();
            self.grace.clear();
            self.cooldown.clear();
            return;
        }

        // grace 到期 → 冻结（收集后执行，避免借用冲突）
        // 每个包携带自己的 grace 时长（per-app strict/覆盖值；缺省全局）
        let mut to_freeze: Vec<String> = Vec::new();
        for (pkg, (start, dur)) in self.grace.iter() {
            if now.duration_since(*start) >= Duration::from_secs(*dur) {
                to_freeze.push(pkg.clone());
            }
        }
        for pkg in to_freeze {
            // 二次校验：已冻结 / 已回前台 / 冷却中 → 跳过
            if self.frozen.contains_key(&pkg) {
                self.grace.remove(&pkg);
                continue;
            }
            if self.last_focus.as_deref() == Some(pkg.as_str()) {
                self.grace.remove(&pkg);
                continue;
            }
            // v0.4.24-l3：内置关键包二次校验（防 focus 噪声/时序漏洞把关键包
            // 放进 grace 后到期冻结——critical 优先级高于一切豁免）
            if self.policy.is_critical(&pkg) {
                logi!("L3 tick critical 豁免跳过: {}", pkg);
                self.events.push_app(
                    EvLevel::Info,
                    EvAction::Exempt,
                    &pkg,
                    Some("tick_critical"),
                    None,
                );
                self.grace.remove(&pkg);
                continue;
            }
            // v0.4.22-l3：VPN 保护（策略热更新后 keep_vpn 开启、VPN 包已在 grace 表）
            if self.is_vpn_protected(&pkg) {
                self.grace.remove(&pkg);
                continue;
            }
            if self.cooldown.contains_key(&pkg) {
                self.grace.remove(&pkg);
                continue;
            }
            // 豁免二次校验（2026-08-02 真机实证补充）：focus 事件在 OPPO ROM 存在
            // pause 噪声（updateActivityUsageStats 的 PAUSED 回调被误报为焦点切换），
            // tick 到期时 last_focus 可能失真；ExemptMonitor 独立线程 2s 节拍以
            // getServices/播放配置真实判定 fg/media——tick 这里兜底消费，防止
            // 有前台服务/媒体播放的 app 被 focus 噪声误冻。
            let (keep_fg, keep_media) = self.keep_flags(&pkg);
            let keep_loc = self.keep_loc(&pkg);
            if let Some(fl) = self.exempt.get(&pkg) {
                if (keep_fg && fl.fg_service) || (keep_media && fl.media) || (keep_loc && fl.location)
                {
                    logi!(
                        "L3 tick豁免跳过（fg={} media={} loc={}）: {}",
                        fl.fg_service, fl.media, fl.location, pkg
                    );
                    self.events.push_app(
                        EvLevel::Info,
                        EvAction::Exempt,
                        &pkg,
                        Some("tick_exempt"),
                        None,
                    );
                    self.grace.remove(&pkg);
                    continue;
                }
            }
            // 高网络负载二次校验（v0.4.19-l3）：grace 到期时仍在高速传输 → 跳过冻结
            if self.keep_hn(&pkg) && self.net_active(&pkg) {
                logi!("L3 tick高网络豁免跳过: {}", pkg);
                self.events.push_app(
                    EvLevel::Info,
                    EvAction::Exempt,
                    &pkg,
                    Some("tick_high_network"),
                    None,
                );
                self.grace.remove(&pkg);
                continue;
            }
            // v0.4.23-l3：网络豁免二次校验——grace 到期时仍有任何网络活动 → 跳过冻结
            if self.keep_net(&pkg) && self.net_active_any(&pkg) {
                logi!("L3 tick网络豁免跳过: {}", pkg);
                self.events.push_app(
                    EvLevel::Info,
                    EvAction::Exempt,
                    &pkg,
                    Some("tick_network_exempt"),
                    None,
                );
                self.grace.remove(&pkg);
                continue;
            }
            self.freeze_now(&pkg, now, "grace_expired");
            self.grace.remove(&pkg);
        }

        // 冷却到期清理
        self.cooldown.retain(|_, until| *until > now);

        // 进程核验（cgroup.procs 内核级）：冻结中但 uid 已无进程 → 移除记录
        // （proc-add 索引不可靠——dex 未上报时恒空，会误杀冻结记录导致"冻着却无记录"）
        let dead: Vec<String> = self
            .frozen
            .keys()
            .filter(|p| match freezer::pkg_uid(p) {
                Some(uid) => !freezer::uid_has_procs(uid),
                None => true, // 包表都查不到 → 视为失效
            })
            .cloned()
            .collect();
        for p in dead {
            self.frozen.remove(&p);
            logw!("L3 冻结记录清理（uid 无进程）: {}", p);
            self.events.push_app(
                EvLevel::Warn,
                EvAction::Unfreeze,
                &p,
                Some("no_procs"),
                None,
            );
        }

        // v0.4.22-l3：冻结表与实际 cgroup 状态对账（每 30 tick ≈9s）——
        // 残留冻结（表无记录但实际冻着）一律解冻，防"冻着却无记录"的僵尸状态。
        // （daemon 重启残留由 main 启动清理兜底；此处覆盖运行期异常）
        self.tick_count += 1;
        if self.tick_count % 30 == 0 {
            let frozen_uids = freezer::frozen_uids();
            if !frozen_uids.is_empty() {
                let mut kept: std::collections::HashSet<u32> = std::collections::HashSet::new();
                for pkg in self.frozen.keys() {
                    if let Some(uid) = freezer::pkg_uid(pkg) {
                        kept.insert(uid);
                    }
                }
                for uid in frozen_uids {
                    if !kept.contains(&uid) && freezer::unfreeze_uid(uid) {
                        logw!("L3 对账解冻残留冻结: uid={}", uid);
                        self.events.push_system(
                            EvLevel::Warn,
                            EvAction::Unfreeze,
                            Some("residual"),
                            Some(&format!("uid={}", uid)),
                        );
                    }
                }
            }
        }

        // v0.4.23-l3：冻结中网络唤醒（对齐 AStop allow_network_wakeup）——keep_network
        // 开启的 app 被冻结后，若内核侧仍有网络流量（rx 计数：外部发包/心跳/隧道流量，
        // 即使进程被冻结内核照收）→ 立即解冻，防"冻死断流"（VPN/推送/下载类）。
        // 每 10 tick ≈3s 一次；解冻后进冷却窗口（防"解冻-再冻"抖动）。
        // 注意：先收集 frozen 键再遍历（net_active_any 需要 &mut self，避免借用冲突）。
        if self.tick_count % 10 == 0 && !self.frozen.is_empty() {
            let frozen_pkgs: Vec<String> = self.frozen.keys().cloned().collect();
            let mut wake: Vec<String> = Vec::new();
            for pkg in &frozen_pkgs {
                if self.keep_net(pkg) && self.net_active_any(pkg) {
                    wake.push(pkg.clone());
                }
            }
            for pkg in wake {
                if freezer::unfreeze_pkg(&pkg) {
                    self.unfreeze_ops += 1;
                    self.wakeup_thaws += 1;
                    logw!("L3 网络唤醒解冻: {}", pkg);
                }
                self.frozen.remove(&pkg);
                self.grace.remove(&pkg);
                self.cooldown
                    .insert(pkg.clone(), Instant::now() + self.cooldown_dur());
                self.events.push_app(
                    EvLevel::Info,
                    EvAction::Unfreeze,
                    &pkg,
                    Some("network_wakeup"),
                    None,
                );
            }
        }
    }

    // ---------------- 决策 ----------------

    /// 该包生效的豁免开关（per-app 覆盖优先，缺省回落全局）
    fn keep_flags(&self, pkg: &str) -> (bool, bool) {
        match self.policy.apps.get(pkg) {
            Some(ap) => (
                ap.keep_fg_service.unwrap_or(self.policy.keep_fg_service),
                ap.keep_media.unwrap_or(self.policy.keep_media),
            ),
            None => (self.policy.keep_fg_service, self.policy.keep_media),
        }
    }

    /// 高网络豁免开关（per-app 覆盖优先，缺省回落全局）
    fn keep_hn(&self, pkg: &str) -> bool {
        match self.policy.apps.get(pkg) {
            Some(ap) => ap.keep_high_network.unwrap_or(self.policy.keep_high_network),
            None => self.policy.keep_high_network,
        }
    }

    /// 网络豁免开关（v0.4.23-l3，per-app 覆盖优先，缺省回落全局）——
    /// 对齐 AStop force_network_exemption：开启 = 有网络活动（任何流量增量）即不冻结，
    /// 已冻结时网络活动触发唤醒解冻（对齐 AStop allow_network_wakeup）。
    fn keep_net(&self, pkg: &str) -> bool {
        match self.policy.apps.get(pkg) {
            Some(ap) => ap.keep_network.unwrap_or(self.policy.keep_network),
            None => self.policy.keep_network,
        }
    }

    /// VPN 守护进程保护（v0.4.22-l3）：keep_vpn 开启时，手动列表或自动探测的
    /// tun 持有者（VPN owner）永不冻结——VPN 被冻结 = 全网断网（实机反馈）。
    /// 优先级最高：白名单/force 之前检查。
    fn is_vpn_protected(&self, pkg: &str) -> bool {
        if !self.policy.keep_vpn {
            return false;
        }
        if self.policy.is_vpn_listed(pkg) {
            return true;
        }
        match crate::freezer::pkg_uid(pkg) {
            Some(uid) => crate::freezer::is_vpn_owner(uid),
            None => false,
        }
    }

    /// 新策略下永不冻结的包（critical / 白名单 / VPN 保护 / per-app exempt）——热更新对账用
    fn should_never_freeze(&self, pkg: &str) -> bool {
        self.policy.is_critical(pkg)
            || self.policy.is_whitelisted(pkg)
            || self.is_vpn_protected(pkg)
            || self
                .policy
                .apps
                .get(pkg)
                .map(|ap| ap.mode == AppMode::Exempt)
                .unwrap_or(false)
    }

    /// 定位活动豁免开关（per-app 覆盖优先，缺省回落全局）
    fn keep_loc(&self, pkg: &str) -> bool {
        match self.policy.apps.get(pkg) {
            Some(ap) => ap.keep_location.unwrap_or(self.policy.keep_location),
            None => self.policy.keep_location,
        }
    }

    /// 交互/FCM 唤醒豁免开关（per-app；缺省 true = 照常解冻，保持既有行为）
    fn keep_wakeup(&self, pkg: &str) -> bool {
        match self.policy.apps.get(pkg) {
            Some(ap) => ap.keep_wakeup.unwrap_or(true),
            None => true,
        }
    }

    /// 子进程策略（per-app 覆盖优先，缺省回落全局 push_policy）
    fn push_mode(&self, pkg: &str) -> PushMode {
        match self.policy.apps.get(pkg) {
            Some(ap) => ap.push_mode.unwrap_or(self.policy.push_policy),
            None => self.policy.push_policy,
        }
    }

    /// 定时解冻窗口判定（per-app unfreeze_window；本地时间分钟数 0..=1439）
    fn in_unfreeze_window(&self, pkg: &str) -> bool {
        let Some(window) = self.policy.apps.get(pkg).and_then(|ap| ap.unfreeze_window) else {
            return false;
        };
        let Some(min) = Self::now_minutes() else {
            return false; // 本地时间获取失败 → 不豁免（宁多冻）
        };
        crate::policy::in_window(min, Some(window))
    }

    /// 高网络负载采样判定：uid 窗口内流量增量 ≥ 阈值（数据源不可用 → false）
    fn net_active(&mut self, pkg: &str) -> bool {
        let Some(uid) = crate::freezer::pkg_uid(pkg) else {
            return false;
        };
        self.net.is_active(uid, DEFAULT_WINDOW, DEFAULT_THRESHOLD)
    }

    /// 网络活动采样判定（v0.4.23-l3）：uid 窗口内任何流量增量 > 0（数据源不可用 → false）。
    /// 内核侧统计：即使进程被 cgroup 冻结，rx 仍计数——冻结中检测到流量即可唤醒。
    fn net_active_any(&mut self, pkg: &str) -> bool {
        let Some(uid) = crate::freezer::pkg_uid(pkg) else {
            return false;
        };
        self.net.is_active_any(uid, DEFAULT_WINDOW)
    }

    /// 当前本地时间（分钟数 0..=1439）；失败 None。libc localtime_r（零依赖）。
    fn now_minutes() -> Option<u32> {
        unsafe {
            let t = libc::time(std::ptr::null_mut());
            if t < 0 {
                return None;
            }
            let mut tm: libc::tm = std::mem::zeroed();
            if libc::localtime_r(&t, &mut tm).is_null() {
                return None;
            }
            Some(tm.tm_hour as u32 * 60 + tm.tm_min as u32)
        }
    }

    /// 旧前台离开：per-app 策略 → 豁免判定 → force 立即冻结 / grace 计时
    fn decide_leave(&mut self, pkg: &str, now: Instant) {
        if !self.policy.enabled {
            return; // 观测模式
        }
        // v0.4.24-l3：内置关键包硬豁免（优先级最高——白名单/force/豁免链之前；
        // 编译期内置清单，配置文件不可覆盖，对齐 AStop critical_apps.txt）
        if self.policy.is_critical(pkg) {
            logi!("L3 critical 保护豁免: {}", pkg);
            self.events.push_app(
                EvLevel::Info,
                EvAction::Exempt,
                pkg,
                Some("critical"),
                None,
            );
            return;
        }
        if self.policy.is_whitelisted(pkg) {
            return; // 白名单永不冻结
        }
        // v0.4.22-l3：VPN 守护进程硬豁免（tun 持有者冻结 = 全网断网，优先级最高）
        if self.is_vpn_protected(pkg) {
            logi!("L3 VPN 保护豁免: {}", pkg);
            self.events.push_app(
                EvLevel::Info,
                EvAction::Exempt,
                pkg,
                Some("vpn_protected"),
                None,
            );
            return;
        }
        // 提前提取 per-app 数据（不持引用——后续 net_active 需要 &mut self）
        let exempt_mode = self
            .policy
            .apps
            .get(pkg)
            .map(|ap| ap.mode == AppMode::Exempt)
            .unwrap_or(false);
        let grace_dur = match self.policy.apps.get(pkg) {
            Some(ap) => ap.effective_grace(self.policy.grace_seconds),
            None => self.policy.grace_seconds,
        };
        // per-app 策略（[apps."pkg"] mode=exempt|standard|strict）
        if exempt_mode {
            logi!("L3 per-app 豁免（mode=exempt）: {}", pkg);
            self.events.push_app(
                EvLevel::Info,
                EvAction::Exempt,
                pkg,
                Some("per_app_exempt"),
                None,
            );
            return;
        }
        // 豁免动作：最近 focus 判定 fg/media/loc（per-app 开关可覆盖全局）
        let (keep_fg, keep_media) = self.keep_flags(pkg);
        let keep_loc = self.keep_loc(pkg);
        if let Some(fl) = self.exempt.get(pkg) {
            if (keep_fg && fl.fg_service) || (keep_media && fl.media) || (keep_loc && fl.location) {
                logi!(
                    "L3 豁免（fg={} media={} loc={}）: {}",
                    fl.fg_service, fl.media, fl.location, pkg
                );
                self.events.push_app(
                    EvLevel::Info,
                    EvAction::Exempt,
                    pkg,
                    Some("exempt_action"),
                    None,
                );
                return;
            }
        }
        // 定时解冻窗口（v0.4.19-l3）：窗口内退后台不冻结（decide_leave 豁免）
        // —— 窗口判定在豁免动作之后：显式动作豁免优先于时间窗口（语义收敛）
        if self.in_unfreeze_window(pkg) {
            logi!("L3 定时窗口豁免: {}", pkg);
            self.events.push_app(
                EvLevel::Info,
                EvAction::Exempt,
                pkg,
                Some("schedule_window"),
                None,
            );
            return;
        }
        // 高网络负载豁免（v0.4.19-l3）：daemon 侧流量采样判定（/proc/uid_stat）
        if self.keep_hn(pkg) && self.net_active(pkg) {
            logi!("L3 高网络豁免: {}", pkg);
            self.events.push_app(
                EvLevel::Info,
                EvAction::Exempt,
                pkg,
                Some("high_network"),
                None,
            );
            return;
        }
        // v0.4.23-l3：网络豁免（对齐 AStop force_network_exemption）——任何网络活动
        // （流量增量 >0，比 keep_high_network 高阈值更宽松）即不冻结。
        // VPN/推送/下载/通话类网络敏感 app 有流量在跑 → 永不进 grace。
        if self.keep_net(pkg) && self.net_active_any(pkg) {
            logi!("L3 网络豁免: {}", pkg);
            self.events.push_app(
                EvLevel::Info,
                EvAction::Exempt,
                pkg,
                Some("network_exempt"),
                None,
            );
            return;
        }
        if self.cooldown.contains_key(pkg) {
            return; // 冷却窗口内免冻
        }
        if self.policy.is_forced(pkg) {
            self.freeze_now(pkg, now, "force");
            return;
        }
        // 离开即计时（已在 grace 中也重置到离开时刻——防止"切回再离开"沿用旧
        // 时刻导致刚离开就被到期冻结）
        self.grace.insert(pkg.to_string(), (now, grace_dur));
        logi!(
            "L3 退后台计时开始: {}（{}s 后冻结）",
            pkg,
            grace_dur
        );
        self.events.push_app(
            EvLevel::Timer,
            EvAction::Delay,
            pkg,
            Some("grace"),
            Some(&format!("{}s", grace_dur)),
        );
    }

    /// 执行冻结（uid 级，经 packages.list 查 uid）
    /// reason: "grace_expired"（tick 到期）| "force"（force 列表立即冻结）——事件语义区分
    /// v0.4.19-l3：push_mode 分派——Keep=选择性冻结（:push 保留）/ Kill=连带杀死 :push
    fn freeze_now(&mut self, pkg: &str, now: Instant, reason: &str) {
        // v0.4.24-l3 最终防线：内置关键包拒绝冻结（防 force/异常路径绕过——critical 优先级最高）
        if self.policy.is_critical(pkg) {
            logw!("L3 critical 保护拒绝冻结: {}", pkg);
            self.events.push_app(
                EvLevel::Warn,
                EvAction::Exempt,
                pkg,
                Some("critical"),
                None,
            );
            self.grace.remove(pkg);
            return;
        }
        // v0.4.22-l3 最终防线：VPN 保护拒绝冻结（防 force 路径绕过）
        if self.is_vpn_protected(pkg) {
            logw!("L3 VPN 保护拒绝冻结: {}", pkg);
            self.events.push_app(
                EvLevel::Warn,
                EvAction::Exempt,
                pkg,
                Some("vpn_protected"),
                None,
            );
            self.grace.remove(pkg);
            return;
        }
        // 冻结前核验：uid 无存活进程 → 跳过（避免无效冻结写与记录混乱）
        match freezer::pkg_uid(pkg) {
            Some(uid) if !freezer::uid_has_procs(uid) => {
                logw!("L3 冻结跳过（uid 无进程）: {}", pkg);
                self.events.push_app(
                    EvLevel::Warn,
                    EvAction::Freeze,
                    pkg,
                    Some("no_procs"),
                    None,
                );
                return;
            }
            _ => {}
        }
        let ok = match self.push_mode(pkg) {
            PushMode::Keep => freezer::freeze_pkg_keep_push(pkg),
            PushMode::Kill => freezer::freeze_pkg_kill_push(pkg),
        };
        if ok {
            self.frozen.insert(pkg.to_string(), now);
            self.freeze_ops += 1;
            logi!("L3 冻结: {}（冻结累计 {}）", pkg, self.freeze_ops);
            self.events.push_app(
                EvLevel::Success,
                EvAction::Freeze,
                pkg,
                Some(reason),
                None,
            );
        } else {
            logw!("L3 冻结执行失败（不加入冻结表）: {}", pkg);
            self.events.push_app(
                EvLevel::Error,
                EvAction::Freeze,
                pkg,
                Some("freeze_failed"),
                None,
            );
        }
    }

    fn cooldown_dur(&self) -> Duration {
        Duration::from_secs(self.policy.cooldown_seconds)
    }

    // ---------------- 状态快照（status 契约） ----------------

    /// 冻结中的包名（排序稳定输出）
    pub fn frozen_packages(&self) -> Vec<String> {
        let mut v: Vec<String> = self.frozen.keys().cloned().collect();
        v.sort();
        v
    }

    /// grace 等待中的包名
    pub fn grace_pending(&self) -> Vec<String> {
        let mut v: Vec<String> = self.grace.keys().cloned().collect();
        v.sort();
        v
    }
}