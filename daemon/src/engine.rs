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

use crate::freezer;
use crate::policy::{AppMode, Policy};
use crate::{logi, logw};

/// 每个包最近一次 focus 事件的豁免判定字段
#[derive(Debug, Clone, Copy)]
pub struct ExemptFlags {
    pub fg_service: bool,
    pub media: bool,
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
    pub freeze_ops: u64,
    pub unfreeze_ops: u64,
    pub wakeup_thaws: u64,
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
            freeze_ops: 0,
            unfreeze_ops: 0,
            wakeup_thaws: 0,
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
        if self.frozen.remove(pkg).is_some() {
            if freezer::unfreeze_pkg(pkg) {
                self.unfreeze_ops += 1;
                logi!("L3 前台解冻: {}（解冻累计 {}）", pkg, self.unfreeze_ops);
            } else {
                logw!("L3 前台解冻失败: {}", pkg);
            }
            self.cooldown.insert(pkg.to_string(), now + self.cooldown_dur());
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
    pub fn on_wakeup(&mut self, pkg: &str) {
        let now = Instant::now();
        if self.frozen.remove(pkg).is_some() {
            if freezer::unfreeze_pkg(pkg) {
                self.unfreeze_ops += 1;
                self.wakeup_thaws += 1;
                logi!("L3 唤醒解冻: {}（累计 {} 次）", pkg, self.wakeup_thaws);
            } else {
                logw!("L3 唤醒解冻失败: {}", pkg);
            }
            self.cooldown.insert(pkg.to_string(), now + self.cooldown_dur());
        }
        // 有唤醒说明进程存活且活跃，取消 pending grace
        self.grace.remove(pkg);
    }

    /// event exempt pkg=P fg=0|1 media=0|1（dex 豁免判定监视器上行，独立线程 2s 节拍）
    pub fn on_exempt(&mut self, pkg: &str, fg: bool, media: bool) {
        self.exempt.insert(
            pkg.to_string(),
            ExemptFlags {
                fg_service: fg,
                media,
            },
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
    }

    /// 策略重载（config.rs reload 回调）：成功替换；失败保留旧表
    pub fn reload_policy(&mut self) {
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
            self.policy = p;
        } else {
            logw!("L3 策略重载失败（保留旧表 revision={}）", self.policy.revision);
        }
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
            if let Some(fl) = self.exempt.get(&pkg) {
                if (keep_fg && fl.fg_service) || (keep_media && fl.media) {
                    logi!(
                        "L3 tick豁免跳过（fg={} media={}）: {}",
                        fl.fg_service, fl.media, pkg
                    );
                    self.grace.remove(&pkg);
                    continue;
                }
            }
            self.freeze_now(&pkg, now);
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

    /// 旧前台离开：per-app 策略 → 豁免判定 → force 立即冻结 / grace 计时
    fn decide_leave(&mut self, pkg: &str, now: Instant) {
        if !self.policy.enabled {
            return; // 观测模式
        }
        if self.policy.is_whitelisted(pkg) {
            return; // 白名单永不冻结
        }
        // per-app 策略（[apps."pkg"] mode=exempt|standard|strict）
        let app_policy = self.policy.apps.get(pkg);
        if let Some(ap) = app_policy {
            if ap.mode == AppMode::Exempt {
                logi!("L3 per-app 豁免（mode=exempt）: {}", pkg);
                return;
            }
        }
        // 豁免动作：最近 focus 判定 fg/media（per-app 开关可覆盖全局）
        let (keep_fg, keep_media) = self.keep_flags(pkg);
        if let Some(fl) = self.exempt.get(pkg) {
            if (keep_fg && fl.fg_service) || (keep_media && fl.media) {
                logi!("L3 豁免（fg={} media={}）: {}", fl.fg_service, fl.media, pkg);
                return;
            }
        }
        if self.cooldown.contains_key(pkg) {
            return; // 冷却窗口内免冻
        }
        if self.policy.is_forced(pkg) {
            self.freeze_now(pkg, now);
            return;
        }
        // grace 时长：per-app（strict 缺省 8s / grace_override）→ 全局
        let grace_dur = match app_policy {
            Some(ap) => ap.effective_grace(self.policy.grace_seconds),
            None => self.policy.grace_seconds,
        };
        // 离开即计时（已在 grace 中也重置到离开时刻——防止"切回再离开"沿用旧
        // 时刻导致刚离开就被到期冻结）
        self.grace.insert(pkg.to_string(), (now, grace_dur));
        logi!(
            "L3 退后台计时开始: {}（{}s 后冻结）",
            pkg,
            grace_dur
        );
    }

    /// 执行冻结（uid 级，经 packages.list 查 uid）
    fn freeze_now(&mut self, pkg: &str, now: Instant) {
        // 冻结前核验：uid 无存活进程 → 跳过（避免无效冻结写与记录混乱）
        match freezer::pkg_uid(pkg) {
            Some(uid) if !freezer::uid_has_procs(uid) => {
                logw!("L3 冻结跳过（uid 无进程）: {}", pkg);
                return;
            }
            _ => {}
        }
        if freezer::freeze_pkg(pkg) {
            self.frozen.insert(pkg.to_string(), now);
            self.freeze_ops += 1;
            logi!("L3 冻结: {}（冻结累计 {}）", pkg, self.freeze_ops);
        } else {
            logw!("L3 冻结执行失败（不加入冻结表）: {}", pkg);
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