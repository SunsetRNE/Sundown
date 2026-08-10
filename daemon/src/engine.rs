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

use std::collections::{HashMap, HashSet};
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
    /// v0.4.29-l3：上次持久化的冻结快照（(pkg, uid) 列表，排序去重）——变化才写盘
    last_persist: Option<Vec<(String, u32)>>,
    /// pkg → (grace 开始时刻, 该包 grace 秒数)——per-app 各自时长（strict 8s / 覆盖值）
    pub grace: HashMap<String, (Instant, u64)>,
    /// pkg → 冷却截止时刻（解冻后免冻窗口）
    pub cooldown: HashMap<String, Instant>,
    /// v0.4.47-l3：保持 OOM 保护（adj=-1000）的候选池——退后台进 grace 即锁定，
    /// 防系统 AppFreezer 抢先 pid 级冻结（与 Sundown 拉锯 → 黑屏，2026-08-05 实机）；
    /// 确认回前台（last_focus）后由 tick 还原 adj 并移出。
    pub adj_keep: std::collections::HashSet<String>,
    /// pkg → 唤醒节流截止时刻（v0.4.42-l3：后台唤醒解冻后窗口内同包不再解冻——
    /// 防 FCM/广播风暴反复"解冻-再冻"抖动，对齐 AStop Probe 60s 限流）
    pub wake_throttle: HashMap<String, Instant>,
    /// 被节流的唤醒次数（status 观测：wake_throttled）
    pub wake_throttled: u64,
    /// pkg → 最近一次豁免判定（focus 事件携带）
    pub exempt: HashMap<String, ExemptFlags>,
    /// 当前前台（恒不冻结）
    pub last_focus: Option<String>,
    /// v0.4.49-l3：动态系统 app 保护集合（pm list packages -s 枚举，启动/热重载刷新）——
    /// 系统组件冻结 = 隐式 Intent/凭据/安装/文件选择等链路黑屏（2026-08-05 相机事故），
    /// 与编译期 CRITICAL_PACKAGES 双保险（pm 失败时回落编译期名单）
    pub system_apps: HashSet<String>,
    /// v0.4.50-l3：android.process.media 的 uid（进程名动态解析——pm 查不到进程，
    /// 相机打开必查 MediaStore → 媒体进程被系统 pid 级冻结 → binder EPIPE → 黑屏；
    /// 启动/热重载时解析，None = 未找到/解析失败）
    pub media_uid: Option<u32>,
    /// v0.4.50-l3：系统链路组件 OOM 锁定去重日志（防 1.5s 周期刷屏）
    pub system_chain_locked: bool,
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
    /// v0.4.52-l3 超时丢弃（行为概念《超时丢弃》）：
    /// 墓碑该做的不是"永远冻着"，而是"冻住 → 过期 → 丢弃 → 释放内存"。
    /// 丢弃 = 冻结终态之一（与"解冻"并列），SIGKILL 整 uid 释放 RSS。
    /// 累计丢弃次数（status discard_ops 观测）
    pub discard_ops: u64,
    /// 各触发原因累计（status discard_reasons 观测）
    pub discard_frozen_timeout: u64,
    pub discard_mem_watermark: u64,
    pub discard_boot_reclaim: u64,
    /// 最近丢弃的包名（去重，上限 20；status discarded_packages 观测）
    pub discarded: Vec<String>,
    /// boot_completed 检测时刻（v0.4.52-l3 开机缓存回收；None = 未检测到）
    pub boot_completed_at: Option<Instant>,
    /// 开机缓存回收是否已执行（只执行一次，防重复回收）
    pub boot_reclaim_done: bool,
    /// 上次会话 Sundown 冻结集（main.rs 启动对账注入）——
    /// boot_reclaim 候选之一（"开机恢复的冻结候选"：有归属证据，Sundown 有权回收）
    pub boot_reclaim_candidates: Vec<(String, u32)>,
}
impl Default for EngineState {
    fn default() -> Self {
        Self {
            policy: Policy::default(),
            frozen: HashMap::new(),
            last_persist: None,
            grace: HashMap::new(),
            cooldown: HashMap::new(),
            adj_keep: std::collections::HashSet::new(),
            wake_throttle: HashMap::new(),
            wake_throttled: 0,
            exempt: HashMap::new(),
            last_focus: None,
            system_apps: HashSet::new(),
            media_uid: None,
            system_chain_locked: false,
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
            discard_ops: 0,
            discard_frozen_timeout: 0,
            discard_mem_watermark: 0,
            discard_boot_reclaim: 0,
            discarded: Vec::new(),
            boot_completed_at: None,
            boot_reclaim_done: false,
            boot_reclaim_candidates: Vec::new(),
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
            // v0.4.47-l3：解冻保持 OOM 保护（adj 暂不还原）——app 已回前台，
            // 系统 OomAdjuster 会立即重算前台 adj（0），此间防系统 AppFreezer 重冻；
            // tick 确认前台后还原（见 tick 段 adj_keep 清理）
            if freezer::unfreeze_pkg_keep_oom(pkg) {
                self.unfreeze_ops += 1;
                self.adj_keep.insert(pkg.to_string());
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
                if freezer::uid_has_frozen_procs(uid) && freezer::unfreeze_pkg_keep_oom(pkg) {
                    self.unfreeze_ops += 1;
                    self.adj_keep.insert(pkg.to_string());
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

    /// event wakeup pkg=P reason=... action=...
    /// v0.4.19-l3：per-app keep_wakeup=false 时忽略唤醒（不解冻不取消 grace——
    /// FCM/交互唤醒风暴 app 保持冻结；事件留痕 reason=wakeup_ignored）。
    /// v0.4.42-l3：wake_throttle_seconds 节流——后台唤醒解冻后窗口内同包再次唤醒
    /// 不解冻（事件留痕 reason=wakeup_throttled），对齐 AStop Probe 60s 限流。
    /// v0.4.43-l3：receiver_gate 广播门控——冻结中 broadcast 源且 action 不在白名单
    /// → 不解冻（留痕 reason=receiver_gated）；service/pendingintent 不受门控；
    /// IMPORTANT 档 app 不受门控（保持"重要"语义）。门控优先于节流。
    /// source：唤醒源（dex 上行 reason=broadcast|service|pendingintent；缺省 "?"）。
    /// action：广播 action（仅 broadcast 源携带；dex 未上报或缺省 "?"）。
    pub fn on_wakeup(&mut self, pkg: &str, source: &str, action: &str) {
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
        // v0.4.43-l3：广播门控——冻结中 + broadcast 源 + action 不在白名单 → 不解冻。
        // 白名单空 = 全部放行（默认零风险）；IMPORTANT 档绕过（保持"重要"语义）；
        // service/pendingintent 源绕过（门控只管广播）。
        let gated = source == "broadcast"
            && !self.policy.receiver_gate.is_empty()
            && self.mode_of(pkg) != AppMode::Important
            && self.frozen.contains_key(pkg)
            && !self.policy.receiver_gate.iter().any(|a| a == action);
        if gated {
            logi!("L3 广播门控: {} action={}（白名单外不解冻）", pkg, action);
            self.events.push_app(
                EvLevel::Info,
                EvAction::Exempt,
                pkg,
                Some("receiver_gated"),
                Some(action),
            );
            return;
        }
        // v0.4.42-l3：唤醒节流——冻结中且窗口内已解冻过 → 本唤醒不解冻（防风暴抖动）。
        // 只拦"解冻动作"，grace 取消照常（进程确实活跃）。用户交互（focus）不走此路径。
        let throttle = self.policy.wake_throttle_seconds;
        if throttle > 0 && self.frozen.contains_key(pkg) {
            if let Some(&until) = self.wake_throttle.get(pkg) {
                if now < until {
                    self.wake_throttled += 1;
                    logi!(
                        "L3 唤醒节流: {} source={}（{}s 窗口内，已节流 {} 次）",
                        pkg,
                        source,
                        throttle,
                        self.wake_throttled
                    );
                    self.events.push_app(
                        EvLevel::Info,
                        EvAction::Exempt,
                        pkg,
                        Some("wakeup_throttled"),
                        Some(source),
                    );
                    return;
                }
            }
        }
        if self.frozen.remove(pkg).is_some() {
            // v0.4.47-l3：keep_oom 解冻（防系统 AppFreezer 立即重冻，见 on_focus 注释）
            if freezer::unfreeze_pkg_keep_oom(pkg) {
                self.unfreeze_ops += 1;
                self.wakeup_thaws += 1;
                self.adj_keep.insert(pkg.to_string());
                // v0.4.42-l3：节流窗口从"实际解冻"起算（仅当开启时记录）
                if throttle > 0 {
                    self.wake_throttle
                        .insert(pkg.to_string(), now + Duration::from_secs(throttle));
                }
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
        // v0.4.47-l3：force-stop → 移出候选池（保险解冻会还原 adj）
        self.adj_keep.remove(pkg);
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

    /// v0.4.49-l3：动态系统 app 保护集合刷新（pm 枚举）——
    /// 系统组件冻结 = 隐式 Intent/凭据/安装/文件选择链路黑屏（2026-08-05 相机事故）。
    /// pm 失败 → 空集（回落编译期 CRITICAL_PACKAGES，零风险降级）。
    /// v0.4.55-l3：`pm list packages -s`（system 标志）改 `-f`（输出安装路径）——
    /// ColorOS 大量系统组件在 /system_ext/ /product/ /vendor/ 分区，-s 标志可能漏；
    /// 按系统分区路径前缀 + 厂商包名域兜底双判定（厂商私有组件更新到 /data 也受保护）。
    pub fn refresh_system_apps(&mut self) {
        let mut fresh = HashSet::new();
        match std::process::Command::new("pm")
            .args(["list", "packages", "-f"])
            .output()
        {
            Ok(out) if out.status.success() => {
                let text = String::from_utf8_lossy(&out.stdout);
                for line in text.lines() {
                    if let Some(pkg) = Self::parse_pm_line(line) {
                        fresh.insert(pkg);
                    }
                }
                logi!("系统 app 保护清单刷新: {} 个（pm -f 分区 + 厂商域判定）", fresh.len());
            }
            _ => {
                logw!("系统 app 枚举失败（回落编译期 critical 名单）");
            }
        }
        self.system_apps = fresh;
        // v0.4.50-l3：android.process.media uid 解析（进程名匹配 /proc cmdline——
        // pm 查不到进程；相机打开必查 MediaStore → 该进程被系统 pid 级冻结 →
        // binder EPIPE(-32) → CameraDeviceImpl onDeviceError → 黑屏，2026-08-05 实锤）
        self.media_uid = Self::resolve_media_uid();
        if let Some(uid) = self.media_uid {
            logi!("android.process.media uid 解析: {}", uid);
        } else {
            logw!("android.process.media uid 解析失败（进程未运行？）");
        }
    }

    /// v0.4.55-l3：系统分区路径判定（pm -f 输出行 path 段）——
    /// /system /vendor /product /odm /system_ext /apex 前缀命中 = 系统组件
    /// （/apex 为 APEX 模块挂载点，同属系统只读分区语义）。纯函数可单测。
    fn is_system_partition_path(path: &str) -> bool {
        let p = path.trim();
        p.starts_with("/system/")
            || p.starts_with("/vendor/")
            || p.starts_with("/product/")
            || p.starts_with("/odm/")
            || p.starts_with("/system_ext/")
            || p.starts_with("/apex/")
    }

    /// v0.4.55-l3：厂商包名域兜底——厂商私有系统组件（OPPO/OnePlus/realme 系）
    /// 可能更新到 /data 分区（分区判定抓不到）或 -f 输出异常；包名域命中即受保护。
    /// 纯函数可单测；与 CRITICAL_PACKAGES（编译期显式清单）双保险。
    fn is_vendor_pkg_domain(pkg: &str) -> bool {
        pkg.starts_with("com.oplus.")
            || pkg.starts_with("com.coloros.")
            || pkg.starts_with("com.oneplus.")
            || pkg.starts_with("com.realme.")
            || pkg.starts_with("com.heytap.")
            || pkg.starts_with("com.nearme.")
    }

    /// v0.4.55-l3：pm -f 输出行解析（纯函数可单测）——
    /// 行格式 `package:<path>=<pkg>`：分区路径前缀命中**或**厂商包名域命中 → Some(pkg)；
    /// 用户分区普通包 / 畸形行 / 空包名 → None（不保护，回落编译期名单语义）。
    fn parse_pm_line(line: &str) -> Option<String> {
        let line = line.trim();
        let pkg = line.rsplit('=').next()?.trim();
        if pkg.is_empty() {
            return None;
        }
        let path = line.strip_prefix("package:").unwrap_or("");
        if Self::is_system_partition_path(path) || Self::is_vendor_pkg_domain(pkg) {
            Some(pkg.to_string())
        } else {
            None
        }
    }

    /// 从 /proc 解析 android.process.media 的 uid（cmdline 首字段匹配 + status Uid）
    fn resolve_media_uid() -> Option<u32> {
        let rd = std::fs::read_dir("/proc").ok()?;
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            // 非数字目录（acpi/cpu/...）跳过，不能提前返回（2026-08-05 v0.4.50 实测 bug）
            let pid: u32 = match name.parse() {
                Ok(p) => p,
                Err(_) => continue,
            };
            let cmdline =
                std::fs::read_to_string(format!("/proc/{}/cmdline", pid)).unwrap_or_default();
            let first = cmdline.split('\0').next().unwrap_or("");
            if first == "android.process.media" {
                let status =
                    std::fs::read_to_string(format!("/proc/{}/status", pid)).unwrap_or_default();
                for line in status.lines() {
                    if let Some(rest) = line.strip_prefix("Uid:") {
                        if let Some(uid) = rest.split_whitespace().next() {
                            if let Ok(u) = uid.parse::<u32>() {
                                return Some(u);
                            }
                        }
                    }
                }
                return None;
            }
        }
        None
    }

    /// v0.4.50-l3：系统链路组件 OOM 锁定（相机黑屏根治）——
    /// 系统 AppFreezer 只冻 adj≥900 的 cached app；对相机交互链路组件（媒体进程/
    /// Intent 解析/凭据/联系人/安装器/相册/相机等）锁 adj=-1000 → 系统冻结逻辑
    /// 直接跳过它们 → 相机打开不再向冻结组件发 binder → 不再 EPIPE 黑屏。
    /// 名单 = CRITICAL_PACKAGES（编译期全量系统组件）+ android.process.media。
    /// protect_oom 幂等：已保护跳过；被 OomAdjuster 覆盖则重写回 -1000。
    fn lock_system_chain_oom(&mut self) {
        for pkg in crate::policy::CRITICAL_PACKAGES {
            if let Some(uid) = freezer::pkg_uid(pkg) {
                freezer::protect_oom(uid);
                // v0.4.50-l3 补丁：防御性解冻残留（保持 -1000）——锁定防"未来冻结"，
                // 但系统在锁定生效前已 pid 级冻结的组件不会自动解（2026-08-05 实测：
                // mms adj=-1000 但 cgroup freeze=1 残留）；双通道 = 锁 + 清残留
                freezer::unfreeze_uid_keep_oom(uid);
            }
        }
        if let Some(uid) = self.media_uid {
            freezer::protect_oom(uid);
            freezer::unfreeze_uid_keep_oom(uid);
        }
        if !self.system_chain_locked {
            self.system_chain_locked = true;
            logi!("系统链路 OOM 锁定已启用（{} 个编译期组件 + media uid={:?}）",
                crate::policy::CRITICAL_PACKAGES.len(), self.media_uid);
        }
    }

    /// 策略重载（config.rs reload 回调）：成功替换；失败保留旧表。
    /// 预设表随热加载一并刷新（action.toml 变更即时生效）；生效中预设仍存在则
    /// 重放覆盖，已删除则回落磁盘参数。
    pub fn reload_policy(&mut self) {
        // v0.4.49-l3：系统 app 保护清单随热加载一并刷新（pm 枚举；失败回落编译期名单）
        self.refresh_system_apps();
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

    /// Sundown 冻结 uid 集（frozen-sync 下行同步源）：冻结表 pkg → uid（pkg_uids 优先，
    /// 包表兜底）；排序去重保证签名稳定。v0.4.27-l3：dex 侧据此区分"HANS 自己冻的"
    /// 与"Sundown 冻的"（两者共用 cgroup.freeze，归属判定是 HANS 防御正确性的前提——
    /// 2026-08-03 误伤事故：误拦 HANS 解冻微信致卡冻结，见 Sundown-HANS误伤事故报告）。
    pub fn sundown_frozen_uids(&self) -> Vec<u32> {
        let mut out: Vec<u32> = Vec::new();
        for pkg in self.frozen.keys() {
            let uid = self
                .pkg_uids
                .get(pkg)
                .copied()
                .or_else(|| crate::freezer::pkg_uid(pkg));
            if let Some(u) = uid {
                out.push(u);
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// v0.4.48-l3 候选池 uid（candidate-sync 下行同步源）：冻结表 + grace 表 + adj_keep 表
    /// 的并集（对齐 AStop hookSystemFreezer 的"被管 app"语义——系统冻结器不得冻结任何
    /// Sundown 正在管理的 app，防止系统在 grace 期抢先 pid 级冻结 → freeze_binder 挂起 →
    /// 黑屏，2026-08-05 实机根因）。排序去重保证签名稳定。
    pub fn sundown_candidate_uids(&self) -> Vec<u32> {
        let mut out: Vec<u32> = Vec::new();
        let mut push_pkg = |pkg: &String| {
            let uid = self
                .pkg_uids
                .get(pkg)
                .copied()
                .or_else(|| crate::freezer::pkg_uid(pkg));
            if let Some(u) = uid {
                out.push(u);
            }
        };
        for pkg in self.frozen.keys() {
            push_pkg(pkg);
        }
        for pkg in self.grace.keys() {
            push_pkg(pkg);
        }
        for pkg in self.adj_keep.iter() {
            push_pkg(pkg);
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// v0.4.29-l3：冻结集持久化（frozen 表变化才写盘）——启动归属对账的权威源。
    /// 行式 `pkg:uid`（零依赖，无 JSON 库）；tick 末尾统一捕获所有 frozen 变化路径
    /// （freeze_now/on_focus/wakeup/force_stop/热更新解冻/网络唤醒/进程核验清理）。
    fn persist_frozen_if_changed(&mut self) {
        let mut snap: Vec<(String, u32)> = Vec::new();
        for pkg in self.frozen.keys() {
            let uid = self
                .pkg_uids
                .get(pkg)
                .copied()
                .or_else(|| crate::freezer::pkg_uid(pkg));
            if let Some(u) = uid {
                snap.push((pkg.clone(), u));
            }
        }
        snap.sort();
        snap.dedup();
        if self.last_persist.as_ref() == Some(&snap) {
            return;
        }
        let mut text = String::new();
        for (p, u) in &snap {
            text.push_str(&format!("{}:{}\n", p, u));
        }
        if std::fs::write(crate::paths::STATE_FROZEN_FILE, text).is_err() {
            logw!("冻结集持久化写盘失败: {}", crate::paths::STATE_FROZEN_FILE);
        }
        self.last_persist = Some(snap);
    }

    /// 周期性推进：grace 到期冻结 / 冷却清理 / 策略关闭全量解冻 / 进程核验
    pub fn tick(&mut self) {
        let now = Instant::now();
        // v0.4.50-l3：系统链路组件 OOM 锁定（相机黑屏根治）——**无条件执行**（放在观测
        // 模式 return 之前）：系统组件保护与策略开关无关，观望模式也必须锁（2026-08-05
        // 实测 bug：放在 enabled 分支内导致观望模式永不锁定，组件 adj 保持 999 被系统冻）
        if self.tick_count % 5 == 0 {
            self.lock_system_chain_oom();
        }
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
            // v0.4.47-l3：策略关闭 → 候选池清空（完整解冻已还原 adj）
            self.adj_keep.clear();
            self.persist_frozen_if_changed(); // v0.4.29-l3：清空冻结集持久化
            // P1⑩（v0.4.38-l3）：观测模式同样落盘事件（全量解冻可审计）
            if self.events.pending_persist() > 0 {
                self.events.persist_new(&crate::paths::current_event_file());
            }
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

        // ============ v0.4.52-l3 超时丢弃（行为概念《超时丢弃》）============
        // 墓碑该做的不是"永远冻着"，而是"冻住 → 过期 → 丢弃 → 释放内存"。
        // 三种触发（均只作用于 Sundown 冻结集，白名单/IMPORTANT/critical/VPN/
        // 系统组件/前台豁免由 discard_pkg 调用前的判定面保证）：
        //   1) frozen_timeout：冻结时长超 [discard] frozen_timeout_seconds 且期间
        //      无任何唤醒命中——"期间零活跃"语义：任何实际解冻（前台/唤醒/网络唤醒）
        //      都会移除 frozen 条目 → 超时自然清零；被节流/门控拦截的唤醒不解冻
        //      → 条目保留 → 超时继续走（防 FCM 风暴把超时无限续期）。
        //   2) mem_watermark：MemAvailable 低于水位 → 按 LRU+RSS 丢弃直到恢复。
        //   3) boot_reclaim：boot_completed 后延迟一次回收 cache 档"开机恢复候选"。

        // 1) 冻结超时丢弃（每 10 tick ≈3s 检查；1800s 默认超时，±3s 粒度可接受）
        if self.tick_count % 10 == 0 && self.policy.discard_frozen_timeout_seconds > 0 {
            let expired = self.expired_discard_candidates(now);
            for pkg in expired {
                self.discard_pkg(&pkg, "frozen_timeout");
            }
        }

        // 2) 内存水位丢弃（每 20 tick ≈6s 采样 meminfo；低于水位 → LRU+RSS 丢弃）
        if self.tick_count % 20 == 0
            && self.policy.discard_mem_watermark_mb > 0
            && !self.frozen.is_empty()
        {
            let watermark_kb = self.policy.discard_mem_watermark_mb.saturating_mul(1024);
            if let Some(avail) = Self::mem_available_kb() {
                if avail < watermark_kb {
                    logw!(
                        "内存水位告急: MemAvailable={}MB < 阈值 {}MB，按 LRU+RSS 丢弃冻结集",
                        avail / 1024,
                        self.policy.discard_mem_watermark_mb
                    );
                    let candidates = self.sort_discard_candidates();
                    for pkg in candidates {
                        // 每丢一个重查水位；已恢复 → 停
                        if Self::mem_available_kb()
                            .map(|a| a >= watermark_kb)
                            .unwrap_or(true)
                        {
                            break;
                        }
                        self.discard_pkg(&pkg, "mem_watermark");
                    }
                }
            }
        }

        // 3) 开机缓存回收（每 100 tick ≈30s 轮询 boot_completed；延迟后执行一次）
        if self.tick_count % 100 == 0 && self.boot_completed_at.is_none() {
            if Self::boot_completed() {
                self.boot_completed_at = Some(Instant::now());
                logi!(
                    "boot_completed 检测到，安排开机缓存回收（{}s 后）",
                    self.policy.discard_boot_reclaim_delay_seconds
                );
            }
        }
        if self.policy.discard_boot_reclaim && !self.boot_reclaim_done {
            if let Some(at) = self.boot_completed_at {
                if now.duration_since(at)
                    >= Duration::from_secs(self.policy.discard_boot_reclaim_delay_seconds)
                {
                    self.boot_reclaim_execute();
                    self.boot_reclaim_done = true;
                }
            }
        }

        // v0.4.46-l3 加固：冻结集 OOM 周期重锁（约 1.5s 一次）——防系统 OomAdjuster/
        // NativeFreezeManager 覆盖冻结 app 的 adj 后脱离保护（被当 cached 反复冻结/被杀）。
        // protect_oom 幂等：已保护跳过；被覆盖则重写回 -1000（backup 仅首次记账）。
        // （系统链路锁定在 tick 开头无条件执行，见上）
        if self.tick_count % 5 == 0 {
            let frozen_pkgs: Vec<String> = self.frozen.keys().cloned().collect();
            for p in &frozen_pkgs {
                if let Some(uid) = freezer::pkg_uid(p) {
                    freezer::protect_oom(uid);
                }
            }
        }

        // v0.4.47-l3：adj_keep 还原——已确认回前台（last_focus）的包还原 OOM 保护
        // （adj 回原值）并移出候选池；仍在后台的保持 -1000（防系统 AppFreezer 重冻）。
        if !self.adj_keep.is_empty() {
            let mut done: Vec<String> = Vec::new();
            for p in self.adj_keep.iter() {
                if self.last_focus.as_deref() == Some(p.as_str()) {
                    if freezer::restore_oom_pkg(p) {
                        logi!("L3 OOM 保护还原（已回前台）: {}", p);
                    }
                    done.push(p.clone());
                }
            }
            for p in done {
                self.adj_keep.remove(&p);
            }
        }

        // v0.4.22-l3 对账（v0.4.29-l3 修复）：原实现扫描 cgroup **全部**冻结 uid 并把
        // 表外冻结一律解冻——HANS/系统冻结的后台进程被误当残留解冻（2026-08-03 实机：
        // enabled=true 时每 9s 解冻微信等 HANS 冻结集，与 HANS 打架）。修复：表外冻结
        // **不动作**（无归属证据不碰，归属判定=冻结集持久化 + 启动对账，见 main.rs）；
        // 表内一致性由下一段"冻结记录清理（uid 无进程）"与冻结集持久化兜底。
        self.tick_count += 1;

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
                // v0.4.47-l3：keep_oom 解冻（网络唤醒的 app 仍在后台，防系统重冻）
                if freezer::unfreeze_pkg_keep_oom(&pkg) {
                    self.unfreeze_ops += 1;
                    self.wakeup_thaws += 1;
                    self.adj_keep.insert(pkg.clone());
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

        // v0.4.29-l3：冻结集持久化（frozen 表变化 → 写盘；启动归属对账的权威源）
        self.persist_frozen_if_changed();

        // P1⑩（v0.4.38-l3）：事件审计持久化——增量水位追加写 JSONL（对齐 AStop
        // firewall_events 时间线）。观测模式也会落盘（全量解冻事件同样可审计）；
        // 失败安全见 events::persist_new（留痕不崩溃，下轮重试）。
        if self.events.pending_persist() > 0 {
            let n = self.events.persist_new(&crate::paths::current_event_file());
            if n > 0 && self.tick_count % 100 == 0 {
                logi!("事件审计: 落盘 {} 条（待同步 {}）", n, self.events.pending_persist());
            }
        }
    }

    // ---------------- 超时丢弃（v0.4.52-l3，行为概念《超时丢弃》） ----------------

    /// 冻结超时丢弃候选：冻结时长 ≥ [discard] frozen_timeout_seconds 的包。
    /// 纯逻辑（frozen 表 + 配置），单测直接覆盖。
    /// 防呆：timeout=0（关闭）→ 恒空（调用方 tick 已前置检查，此处双保险）。
    fn expired_discard_candidates(&self, now: Instant) -> Vec<String> {
        let timeout = self.policy.discard_frozen_timeout_seconds;
        if timeout == 0 {
            return Vec::new();
        }
        self.frozen
            .iter()
            .filter(|(_, since)| now.duration_since(**since) >= Duration::from_secs(timeout))
            .map(|(p, _)| p.clone())
            .collect()
    }

    /// 内存水位丢弃候选排序：LRU（frozen_since 最旧优先）+ RSS 占用（大→小）。
    /// RSS 读取失败（测试环境/进程刚死）按 0 计——排序退化为纯 LRU，安全。
    fn sort_discard_candidates(&self) -> Vec<String> {
        let mut items: Vec<(String, Instant, u64)> = Vec::new();
        for (pkg, since) in &self.frozen {
            let uid = freezer::pkg_uid(pkg).unwrap_or(0);
            let rss = if uid != 0 { Self::uid_rss_kb(uid) } else { 0 };
            items.push((pkg.clone(), *since, rss));
        }
        // 主：frozen_since 旧 → 新（最旧优先丢）；次：RSS 大 → 小（占内存多的先丢）
        items.sort_by(|a, b| a.1.cmp(&b.1).then(b.2.cmp(&a.2)));
        items.into_iter().map(|(p, _, _)| p).collect()
    }

    /// 丢弃资格判定（安全护栏，纯逻辑可单测）：命中者任何丢弃机制不得触碰——
    /// 白名单 / per-app exempt / critical 内置 / 动态系统组件 / VPN 硬豁免 /
    /// IMPORTANT 档 / 当前前台。复用 v0.4.49 三处接入的既有判定面。
    /// v0.4.55-l3 落刀前终检：exempt 表实时判定（fg_service/media/location）——
    /// 与 tick 冻结路径的豁免二次校验（keep_flags/keep_loc + exempt 表）同构。
    /// 2026-08-11 联通 ANR 事件实机印证：焦点抖动可致 last_focus 失真（00:39-00:41
    /// 17 次焦点切换），此时持有前台服务/媒体/定位的冻结 app 不得被 SIGKILL 丢弃。
    fn discard_ineligible(&self, pkg: &str) -> bool {
        if self.should_never_freeze(pkg)
            || self.mode_of(pkg) == AppMode::Important
            || self.last_focus.as_deref() == Some(pkg)
        {
            return true;
        }
        // 落刀前终检：exempt 表（独立线程 2s 节拍实时判定）——与 tick 冻结路径同构
        let (keep_fg, keep_media) = self.keep_flags(pkg);
        let keep_loc = self.keep_loc(pkg);
        if let Some(fl) = self.exempt.get(pkg) {
            if (keep_fg && fl.fg_service) || (keep_media && fl.media) || (keep_loc && fl.location) {
                return true;
            }
        }
        false
    }

    /// 执行丢弃一个包（终态：SIGKILL 整 uid 释放内存，不可撤销）。
    /// 失败安全：
    ///   - uid 定位失败（包表未知）→ 清理记录不动作（包已不存在）；
    ///   - 归属核验无进程 → 清理记录（无尸体可丢，等价 no_procs 清理语义）；
    ///   - SIGKILL 后竞态无进程 → 同样清理记录。
    /// 成功 → 清理全部相关记录（frozen/grace/cooldown/adj_keep）+ 计数 + 事件留痕
    /// （discard pkg=P reason=...，JSONL 审计随事件持久化落盘）。
    fn discard_pkg(&mut self, pkg: &str, reason: &str) -> bool {
        // 最终防线：安全护栏（调用方通常已过滤，这里兜底防 force/异常路径绕过）
        if self.discard_ineligible(pkg) {
            logw!("L3 丢弃护栏拒绝: {}（reason={}）", pkg, reason);
            return false;
        }
        let Some(uid) = freezer::pkg_uid(pkg) else {
            logw!("L3 丢弃跳过（包表未知）: {}", pkg);
            self.frozen.remove(pkg);
            return false;
        };
        if !freezer::uid_has_procs(uid) {
            // 无尸体可丢（进程已死）——清理记录（与 no_procs 同语义）
            self.frozen.remove(pkg);
            return false;
        }
        let killed = freezer::discard_uid(uid);
        if killed == 0 {
            // 归属核验后仍无进程（竞态）→ 清理记录
            self.frozen.remove(pkg);
            return false;
        }
        self.frozen.remove(pkg);
        self.grace.remove(pkg);
        self.cooldown.remove(pkg);
        self.adj_keep.remove(pkg);
        self.discard_ops += 1;
        match reason {
            "frozen_timeout" => self.discard_frozen_timeout += 1,
            "mem_watermark" => self.discard_mem_watermark += 1,
            "boot_reclaim" => self.discard_boot_reclaim += 1,
            _ => {}
        }
        // 最近丢弃列表（去重，上限 20）
        self.discarded.retain(|p| p != pkg);
        self.discarded.push(pkg.to_string());
        if self.discarded.len() > 20 {
            self.discarded.remove(0);
        }
        logw!(
            "L3 丢弃: {} uid={} reason={}（SIGKILL {} 进程，累计丢弃 {}）",
            pkg,
            uid,
            reason,
            killed,
            self.discard_ops
        );
        self.events.push_app(
            EvLevel::Warn,
            EvAction::Discard,
            pkg,
            Some(reason),
            Some(&format!("uid={} killed={}", uid, killed)),
        );
        true
    }

    /// 开机缓存回收执行（boot_completed + 延迟到期后调用一次）：
    /// 候选 = 上次会话 Sundown 冻结集（启动对账注入，有归属证据）∪ 当前冻结集；
    /// 只回收 cache/empty 档（包全部存活进程 oom_score_adj ≥ 900）——当前冻结集
    /// 已被 OOM 锁定 -1000 天然排除（防"刚冻就被开机回收杀"），主要命中开机时
    /// 系统自动恢复的"上次冻结过的包"（高缓存痛点直击）。
    fn boot_reclaim_execute(&mut self) {
        if self.boot_reclaim_candidates.is_empty() && self.frozen.is_empty() {
            logi!("开机缓存回收：无候选（上次会话无冻结集）");
            return;
        }
        // 候选集合（去重）
        let mut set: HashSet<String> = HashSet::new();
        for (p, _) in &self.boot_reclaim_candidates {
            set.insert(p.clone());
        }
        for p in self.frozen.keys() {
            set.insert(p.clone());
        }
        // 安全护栏过滤 + cache 档判定
        let mut reclaim: Vec<String> = Vec::new();
        for p in set {
            if self.discard_ineligible(&p) {
                continue;
            }
            if !Self::pkg_is_cache_adj(&p) {
                continue; // 非 cache/empty 档（前台/感知/服务/已 OOM 锁定）不动
            }
            reclaim.push(p);
        }
        if reclaim.is_empty() {
            logi!("开机缓存回收：无 cache 档候选，跳过");
            return;
        }
        reclaim.sort();
        logw!("开机缓存回收：{} 个 cache 档候选（{}）", reclaim.len(), reclaim.join(","));
        for pkg in reclaim {
            self.discard_pkg(&pkg, "boot_reclaim");
        }
    }

    /// /proc/meminfo MemAvailable（kB）。Android 内核普遍存在；读不到 → None
    /// （水位丢弃跳过，保守不动作）。
    fn mem_available_kb() -> Option<u64> {
        let text = std::fs::read_to_string("/proc/meminfo").ok()?;
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("MemAvailable:") {
                return rest.split_whitespace().next()?.parse().ok();
            }
        }
        None
    }

    /// uid 全部进程 RSS 合计（kB）：/proc/<pid>/statm 第二字段（resident pages）× 4KB
    /// （arm64 固定 page size；读失败按 0——排序退化纯 LRU，安全）
    fn uid_rss_kb(uid: u32) -> u64 {
        let mut total = 0u64;
        for pid in freezer::uid_pids(uid) {
            if let Ok(s) = std::fs::read_to_string(format!("/proc/{}/statm", pid)) {
                if let Some(res) = s.split_whitespace().nth(1) {
                    if let Ok(pages) = res.parse::<u64>() {
                        total += pages.saturating_mul(4);
                    }
                }
            }
        }
        total
    }

    /// 包是否整体处于 cache/empty 档（所有存活进程 oom_score_adj ≥ 缓存档阈值 900）。
    /// 读不到（无进程/未知 uid/adj 不可读）→ false（不回收，保守）。
    fn pkg_is_cache_adj(pkg: &str) -> bool {
        let Some(uid) = freezer::pkg_uid(pkg) else {
            return false;
        };
        let pids = freezer::uid_pids(uid);
        if pids.is_empty() {
            return false;
        }
        pids.iter().all(|pid| {
            std::fs::read_to_string(format!("/proc/{}/oom_score_adj", pid))
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok())
                .map(|adj| adj >= 900)
                .unwrap_or(false)
        })
    }

    /// sys.boot_completed 属性（getprop；daemon root 可读）。
    /// 失败 → false（不安排回收，保守；下轮重试）。
    fn boot_completed() -> bool {
        std::process::Command::new("getprop")
            .arg("sys.boot_completed")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "1")
            .unwrap_or(false)
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

    /// 新策略下永不冻结的包（critical / 系统 app / 白名单 / VPN 保护 / per-app exempt）——热更新对账用
    fn should_never_freeze(&self, pkg: &str) -> bool {
        self.policy.is_critical(pkg)
            || self.system_apps.contains(pkg)
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
    /// v0.4.41-l3：important 档强制开启（唤醒即解冻——"重要但可冻结"语义，
    /// 防止配置 keep_wakeup=false 把重要 app 冻死在墓碑里）
    fn keep_wakeup(&self, pkg: &str) -> bool {
        match self.policy.apps.get(pkg) {
            Some(ap) => {
                if ap.mode == AppMode::Important {
                    return true;
                }
                ap.keep_wakeup.unwrap_or(true)
            }
            None => true,
        }
    }

    /// per-app 档位（v0.4.43-l3，Receiver gate 判定用；无配置 = Standard）
    fn mode_of(&self, pkg: &str) -> AppMode {
        self.policy
            .apps
            .get(pkg)
            .map(|ap| ap.mode)
            .unwrap_or(AppMode::Standard)
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
        // v0.4.49-l3：动态系统 app 保护（pm 枚举集合）——系统组件冻结 = 隐式 Intent/
        // 凭据/安装/文件选择链路黑屏（2026-08-05 相机事故：intentresolver 被冻 →
        // 第三方调相机黑屏）。优先级仅次于 critical（高于白名单——系统组件永不可冻）
        if self.system_apps.contains(pkg) {
            logi!("L3 系统组件保护豁免: {}", pkg);
            self.events.push_app(
                EvLevel::Info,
                EvAction::Exempt,
                pkg,
                Some("system_app"),
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
        // v0.4.47-l3：退后台进 grace 即锁 OOM（adj=-1000）——系统 AppFreezer 只冻结
        // cached（adj≥阈值）app；不锁则系统会抢先 pid 级冻结候选 app（Sundown 解冻后
        // 系统立即重冻 → 拉锯 → 黑屏，2026-08-05 实机根因）。protect_oom 幂等；
        // 解冻后由 tick 在确认回前台时还原（见 tick 段 adj_keep 清理）。
        if let Some(uid) = freezer::pkg_uid(pkg) {
            freezer::protect_oom(uid);
        }
        self.adj_keep.insert(pkg.to_string());
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
        // v0.4.49-l3 最终防线：动态系统 app 拒绝冻结（防 force/异常路径绕过——与 critical 同级）
        if self.system_apps.contains(pkg) {
            logw!("L3 系统组件保护拒绝冻结: {}", pkg);
            self.events.push_app(
                EvLevel::Warn,
                EvAction::Exempt,
                pkg,
                Some("system_app"),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// v0.4.42-l3：唤醒节流——窗口内同包后台唤醒不解冻（防 FCM/广播风暴抖动）。
    /// 不依赖真实 cgroup：直接预置 wake_throttle 窗口，验证 frozen 表/计数/事件留痕。
    #[test]
    fn wake_throttle_v042() {
        let mut e = EngineState::default();
        e.policy.wake_throttle_seconds = 60;

        // ① 冻结中 + 窗口内（模拟刚解冻过）→ 唤醒被节流：frozen 保留、计数 +1、事件 wakeup_throttled
        e.frozen.insert("com.storm.app".to_string(), Instant::now());
        e.wake_throttle
            .insert("com.storm.app".to_string(), Instant::now() + Duration::from_secs(60));
        e.on_wakeup("com.storm.app", "broadcast", "android.intent.action.SCREEN_ON");
        assert!(e.frozen.contains_key("com.storm.app"), "窗口内不解冻");
        assert_eq!(e.wake_throttled, 1);
        let j = e.events.to_json(8);
        assert!(j.contains("wakeup_throttled"), "事件留痕: {}", j);
        assert!(j.contains("broadcast"), "携带唤醒源: {}", j);

        // ② 窗口过期 → 唤醒放行解冻（frozen 移除；unfreeze 在测试环境失败不影响 frozen 清理）
        e.wake_throttle.insert(
            "com.storm.app".to_string(),
            Instant::now() - Duration::from_secs(1),
        );
        e.on_wakeup("com.storm.app", "pendingintent", "?");
        assert!(!e.frozen.contains_key("com.storm.app"), "窗口过期后解冻");

        // ③ throttle=0（关闭节流）→ 即使窗口有记录也不拦
        let mut e2 = EngineState::default();
        e2.policy.wake_throttle_seconds = 0;
        e2.frozen.insert("com.storm.app".to_string(), Instant::now());
        e2.wake_throttle
            .insert("com.storm.app".to_string(), Instant::now() + Duration::from_secs(60));
        e2.on_wakeup("com.storm.app", "service", "?");
        assert!(!e2.frozen.contains_key("com.storm.app"), "关闭节流不拦");
        assert_eq!(e2.wake_throttled, 0);

        // ④ keep_wakeup=false 优先于节流（直接忽略）
        let mut e3 = EngineState::default();
        e3.policy.wake_throttle_seconds = 60;
        e3.policy.apps.insert(
            "com.storm.app".to_string(),
            crate::policy::AppPolicy {
                mode: AppMode::Standard,
                keep_wakeup: Some(false),
                ..Default::default()
            },
        );
        e3.frozen.insert("com.storm.app".to_string(), Instant::now());
        e3.on_wakeup("com.storm.app", "broadcast", "?");
        assert!(e3.frozen.contains_key("com.storm.app"));
        assert_eq!(e3.wake_throttled, 0);
        let j3 = e3.events.to_json(8);
        assert!(j3.contains("wakeup_ignored"), "keep_wakeup=false 走 ignored: {}", j3);
    }

    /// v0.4.43-l3：Receiver gate 广播门控——白名单外 broadcast 不解冻；
    /// 空白名单全放行（零风险默认）；IMPORTANT 档绕过；service 源不受门控。
    #[test]
    fn receiver_gate_v043() {
        // ① gate 为空（默认）→ broadcast 正常解冻（现状兼容）
        let mut e = EngineState::default();
        e.frozen.insert("com.gate.app".to_string(), Instant::now());
        e.on_wakeup("com.gate.app", "broadcast", "android.intent.action.ANY");
        assert!(!e.frozen.contains_key("com.gate.app"), "空门控全放行");
        assert_eq!(e.events.to_json(8).matches("receiver_gated").count(), 0);

        // ② gate 非空 + 白名单 action → 解冻
        let mut e2 = EngineState::default();
        e2.policy.receiver_gate = vec!["android.intent.action.USER_PRESENT".to_string()];
        e2.frozen.insert("com.gate.app".to_string(), Instant::now());
        e2.on_wakeup("com.gate.app", "broadcast", "android.intent.action.USER_PRESENT");
        assert!(!e2.frozen.contains_key("com.gate.app"), "白名单 action 放行");

        // ③ gate 非空 + 非白名单 action → 不解冻 + receiver_gated 留痕
        let mut e3 = EngineState::default();
        e3.policy.receiver_gate = vec!["android.intent.action.USER_PRESENT".to_string()];
        e3.frozen.insert("com.gate.app".to_string(), Instant::now());
        e3.on_wakeup("com.gate.app", "broadcast", "com.vendor.SPAM");
        assert!(e3.frozen.contains_key("com.gate.app"), "白名单外不解冻");
        let j3 = e3.events.to_json(8);
        assert!(j3.contains("receiver_gated"), "留痕 receiver_gated: {}", j3);
        assert!(j3.contains("com.vendor.SPAM"), "携带 action: {}", j3);

        // ④ gate 非空 + IMPORTANT 档 → 绕过门控解冻（保持"重要"语义）
        let mut e4 = EngineState::default();
        e4.policy.receiver_gate = vec!["android.intent.action.USER_PRESENT".to_string()];
        e4.policy.apps.insert(
            "com.tencent.mm".to_string(),
            crate::policy::AppPolicy {
                mode: AppMode::Important,
                ..Default::default()
            },
        );
        e4.frozen.insert("com.tencent.mm".to_string(), Instant::now());
        e4.on_wakeup("com.tencent.mm", "broadcast", "com.vendor.SPAM");
        assert!(!e4.frozen.contains_key("com.tencent.mm"), "IMPORTANT 档绕过门控");

        // ⑤ gate 非空 + service 源 → 不受门控（门控只管广播）
        let mut e5 = EngineState::default();
        e5.policy.receiver_gate = vec!["android.intent.action.USER_PRESENT".to_string()];
        e5.frozen.insert("com.gate.app".to_string(), Instant::now());
        e5.on_wakeup("com.gate.app", "service", "?");
        assert!(!e5.frozen.contains_key("com.gate.app"), "service 源不受门控");
    }

    /// v0.4.52-l3：冻结超时丢弃——过期判定（纯逻辑）+ 失败安全路径。
    /// 真实 cgroup/SIGKILL 由真机验证；此处覆盖状态机与护栏。
    #[test]
    fn frozen_timeout_discard_v052() {
        let mut e = EngineState::default();
        e.policy.enabled = true;
        e.policy.discard_frozen_timeout_seconds = 100;

        // ① 过期判定：刚冻结（未超时）不在候选；150s 前冻结（超时）命中
        e.frozen.insert("com.fresh.app".to_string(), Instant::now());
        e.frozen.insert(
            "com.stale.app".to_string(),
            Instant::now() - Duration::from_secs(150),
        );
        let expired = e.expired_discard_candidates(Instant::now());
        assert_eq!(expired, vec!["com.stale.app"], "仅超时项命中");

        // ② timeout=0（关闭）→ 恒无候选
        e.policy.discard_frozen_timeout_seconds = 0;
        assert!(
            e.expired_discard_candidates(Instant::now()).is_empty(),
            "关闭丢弃不产生候选"
        );

        // ③ 失败安全：包表未知（测试环境无 packages.list）→ 清理记录、不计数、无 panic
        e.policy.discard_frozen_timeout_seconds = 100;
        let ok = e.discard_pkg("com.stale.app", "frozen_timeout");
        assert!(!ok, "包表未知 → 丢弃失败");
        assert!(!e.frozen.contains_key("com.stale.app"), "记录已清理");
        assert_eq!(e.discard_ops, 0, "失败不计数");
        assert_eq!(e.discard_frozen_timeout, 0);

        // ④ 护栏拒绝：白名单包即使超时也不动（记录保留 + 不计数）
        e.frozen.insert("com.whitelisted.app".to_string(), Instant::now());
        e.policy.whitelist = vec!["com.whitelisted.app".to_string()];
        let ok2 = e.discard_pkg("com.whitelisted.app", "frozen_timeout");
        assert!(!ok2, "护栏拒绝");
        assert!(e.frozen.contains_key("com.whitelisted.app"), "白名单保留");
        assert_eq!(e.discard_ops, 0);
    }

    /// v0.4.52-l3：内存水位丢弃——候选排序（LRU 最旧优先）+ 护栏判定。
    #[test]
    fn mem_watermark_discard_v052() {
        let mut e = EngineState::default();
        e.policy.enabled = true;

        // ① LRU 排序：最旧先丢（RSS 测试环境读不到按 0，排序退化为纯 LRU）
        e.frozen.insert("com.new.app".to_string(), Instant::now());
        e.frozen.insert(
            "com.mid.app".to_string(),
            Instant::now() - Duration::from_secs(60),
        );
        e.frozen.insert(
            "com.old.app".to_string(),
            Instant::now() - Duration::from_secs(600),
        );
        let order = e.sort_discard_candidates();
        assert_eq!(order, vec!["com.old.app", "com.mid.app", "com.new.app"], "LRU 最旧优先");

        // ② 护栏：白名单 / exempt / critical / 系统组件 / VPN / IMPORTANT / 前台 → 不可丢弃
        e.policy.whitelist = vec!["com.wl.app".to_string()];
        e.policy.apps.insert(
            "com.imp.app".to_string(),
            crate::policy::AppPolicy {
                mode: AppMode::Important,
                ..Default::default()
            },
        );
        e.policy.apps.insert(
            "com.exempt.app".to_string(),
            crate::policy::AppPolicy {
                mode: AppMode::Exempt,
                ..Default::default()
            },
        );
        e.last_focus = Some("com.fg.app".to_string());
        e.system_apps.insert("com.sys.app".to_string());
        assert!(e.discard_ineligible("com.wl.app"), "白名单");
        assert!(e.discard_ineligible("com.imp.app"), "IMPORTANT");
        assert!(e.discard_ineligible("com.exempt.app"), "per-app exempt");
        assert!(e.discard_ineligible("com.android.systemui"), "critical 内置");
        assert!(e.discard_ineligible("com.sys.app"), "动态系统组件");
        assert!(e.discard_ineligible("com.fg.app"), "当前前台");
        assert!(!e.discard_ineligible("com.normal.app"), "标准档可丢弃");
    }

    /// v0.4.52-l3：开机缓存回收——状态机（延迟到期执行一次 + 无候选安全跳过）。
    /// cache 档 adj 判定依赖真实 /proc（测试环境恒 false → 候选恒空，验证安全跳过）。
    #[test]
    fn boot_reclaim_v052() {
        let mut e = EngineState::default();
        e.policy.enabled = true;
        e.policy.discard_boot_reclaim = true;
        e.policy.discard_boot_reclaim_delay_seconds = 0;

        // ① boot_completed 未检测到 → 不执行
        e.boot_completed_at = None;
        assert!(!e.boot_reclaim_done);
        e.tick_count = 100;
        // tick 中 getprop 在测试环境失败 → boot_completed_at 保持 None → 不安排
        e.tick();
        assert!(e.boot_completed_at.is_none(), "getprop 失败不安排回收");

        // ② 手动置 boot_completed_at（模拟已检测到）→ 延迟 0 到期 → tick 执行一次
        e.boot_completed_at = Some(Instant::now() - Duration::from_secs(1));
        e.boot_reclaim_candidates = vec![
            ("com.oldfrozen.app".to_string(), 10001),
            ("com.android.settings".to_string(), 10002), // critical 内置，护栏排除
        ];
        e.tick_count = 100; // %100==0 节拍（boot 检测段：boot_completed_at 已置 → 跳过检测，直接走执行段）
        e.tick();
        assert!(e.boot_reclaim_done, "tick 执行后标记完成");
        assert_eq!(e.discard_ops, 0, "测试环境无 cache 档候选，零丢弃");

        // ③ 只执行一次（done=true 后不再执行）
        let before = e.discard_ops;
        e.tick_count = 200;
        e.tick();
        assert_eq!(e.discard_ops, before, "重复 tick 不重复执行");
    }

    /// v0.4.55-l3：系统分区路径判定（pm -f 改造核心）——
    /// 6 个系统分区前缀命中；用户分区/相似前缀无尾斜杠/空串不命中；容忍首尾空白。
    #[test]
    fn system_partition_path_v055() {
        // 命中：/system /vendor /product /odm /system_ext /apex 六前缀（pm -f 真实行形态）
        for p in [
            "/system/priv-app/Settings/Settings.apk",
            "/system/framework/framework-res.apk",
            "/vendor/app/com.oplus.xxx/xxx.apk",
            "/product/overlay/com.coloros.yyy/yyy.apk",
            "/odm/etc/com.oplus.zzz/zzz.apk",
            "/system_ext/priv-app/com.android.aaa/aaa.apk",
            "/apex/com.android.art/art.apk",
        ] {
            assert!(EngineState::is_system_partition_path(p), "分区前缀应命中: {}", p);
        }
        // 不命中：用户分区 / 存储 / 无尾斜杠前缀（防误判整词）/ 空串 / 无前导斜杠
        for p in [
            "/data/app/com.example.foo-1/base.apk",
            "/data/user/0/com.example.foo/base.apk",
            "/storage/emulated/0/Download/x.apk",
            "/system",
            "/vendor",
            "/product",
            "/odm",
            "/system_ext",
            "/apex",
            "",
            "system/priv-app/x.apk",
        ] {
            assert!(!EngineState::is_system_partition_path(p), "非分区不应命中: {}", p);
        }
        // trim 容忍（pm 输出行可能带前导空格）
        assert!(EngineState::is_system_partition_path("  /system/priv-app/x.apk "));
    }

    /// v0.4.55-l3：厂商包名域兜底——6 个厂商域命中（含更新到 /data 的私有组件）；
    /// 域前缀无点（如 com.oplus 框架包本身）、非厂商域、空串不命中。
    #[test]
    fn vendor_pkg_domain_v055() {
        for p in [
            "com.oplus.phone",
            "com.coloros.phonemanager",
            "com.oneplus.settings",
            "com.realme.movie",
            "com.heytap.openid",
            "com.nearme.instant",
        ] {
            assert!(EngineState::is_vendor_pkg_domain(p), "厂商域应命中: {}", p);
        }
        for p in [
            "com.android.settings", // AOSP 域（走分区判定，不靠域兜底）
            "com.tencent.mm",       // 第三方
            "com.oplus",            // 域前缀无点 = 框架包本身，非组件（分区判定兜住）
            "com.coloros",
            "com.oneplus",
            "com.realme",
            "com.heytap",
            "com.nearme",
            "oplus.phone", // 缺 com. 前缀
            "",
        ] {
            assert!(!EngineState::is_vendor_pkg_domain(p), "非厂商域不应命中: {}", p);
        }
    }

    /// v0.4.55-l3：pm -f 输出行解析——双判定组合（分区路径 OR 厂商域）：
    /// 用户分区普通包不保护；厂商私有组件更新到 /data 仍保护；畸形行安全 None。
    #[test]
    fn parse_pm_line_v055() {
        // ① 系统分区命中
        assert_eq!(
            EngineState::parse_pm_line("package:/system/priv-app/Settings/Settings.apk=com.android.settings"),
            Some("com.android.settings".to_string())
        );
        // ② /system_ext 分区命中（-s 标志漏项的核心修复点）
        assert_eq!(
            EngineState::parse_pm_line("package:/system_ext/priv-app/com.android.aaa/aaa.apk=com.android.aaa"),
            Some("com.android.aaa".to_string())
        );
        // ③ 用户分区 + 厂商域兜底命中（更新到 /data 的私有组件）
        assert_eq!(
            EngineState::parse_pm_line("package:/data/app/~~abc==/com.coloros.phonemanager-1/base.apk=com.coloros.phonemanager"),
            Some("com.coloros.phonemanager".to_string())
        );
        // ④ 用户分区普通包不保护
        assert_eq!(
            EngineState::parse_pm_line("package:/data/app/~~abc==/com.example.foo-1/base.apk=com.example.foo"),
            None
        );
        // ⑤ 畸形行 / 空包名 / 空行安全 None（不 panic）
        assert_eq!(EngineState::parse_pm_line("garbage line"), None);
        assert_eq!(EngineState::parse_pm_line("package:/system/priv-app/x.apk="), None);
        assert_eq!(EngineState::parse_pm_line(""), None);
        assert_eq!(EngineState::parse_pm_line("   "), None);
    }

    /// v0.4.55-l3：discard 落刀前 exempt 表终检——持有前台服务/媒体/定位的
    /// 冻结 app 不得被 SIGKILL 丢弃（与 tick 冻结路径豁免二次校验同构）。
    /// 2026-08-11 联通 ANR 实机背书：焦点抖动（00:39-00:41 17 次切换）可致
    /// last_focus 失真，此时 exempt 表实时判定是唯一可靠豁免源。
    #[test]
    fn discard_exempt_final_check_v055() {
        let mut e = EngineState::default();
        e.policy.enabled = true;
        e.policy.keep_fg_service = true;
        e.policy.keep_media = true;
        e.policy.keep_location = true;

        // ① 前台服务（fg_service=true）→ 终检拦截
        e.on_exempt("com.service.app", true, false, false);
        assert!(e.discard_ineligible("com.service.app"), "fg_service 终检拦截");

        // ② 媒体播放（media=true）→ 终检拦截
        e.on_exempt("com.music.app", false, true, false);
        assert!(e.discard_ineligible("com.music.app"), "media 终检拦截");

        // ③ 定位活动（location=true）→ 终检拦截
        e.on_exempt("com.nav.app", false, false, true);
        assert!(e.discard_ineligible("com.nav.app"), "location 终检拦截");

        // ④ 无豁免标记（exempt 表 reason=none）→ 标准档可丢弃
        e.on_exempt("com.normal.app", false, false, false);
        assert!(!e.discard_ineligible("com.normal.app"), "无豁免可丢弃");

        // ⑤ 豁免开关关闭（keep_* = false）→ 即使 exempt 表标记 true 也不拦截（配置决定）
        let mut e2 = EngineState::default();
        e2.policy.enabled = true;
        e2.policy.keep_fg_service = false;
        e2.policy.keep_media = false;
        e2.policy.keep_location = false;
        e2.on_exempt("com.service.app", true, true, true);
        assert!(!e2.discard_ineligible("com.service.app"), "豁免开关关闭不拦截");

        // ⑥ exempt 表无记录（dex 未上报/上报丢失）→ 不拦截（仅当有记录才判定）
        let mut e3 = EngineState::default();
        e3.policy.enabled = true;
        e3.policy.keep_fg_service = true;
        assert!(!e3.discard_ineligible("com.unknown.app"), "无 exempt 记录不拦截");

        // ⑦ 联通 ANR 场景复现：焦点抖动致 last_focus 失真（最后焦点为 launcher），
        //    联通仍持有前台服务 → exempt 终检兜底拦截落刀
        let mut e4 = EngineState::default();
        e4.policy.enabled = true;
        e4.policy.keep_fg_service = true;
        e4.last_focus = Some("com.oplus.launcher".to_string());
        e4.on_exempt("com.sinovatech.unicom.ui", true, false, false);
        assert!(
            e4.discard_ineligible("com.sinovatech.unicom.ui"),
            "焦点失真时 exempt 终检兜底（联通 ANR 场景）"
        );
    }

    /// v0.4.55-l3：refresh_system_apps 失败安全——pm 不可用（测试环境）→
    /// 空集回落（编译期 CRITICAL_PACKAGES 双保险由 should_never_freeze 保证），不 panic。
    #[test]
    fn refresh_system_apps_fallback_v055() {
        let mut e = EngineState::default();
        e.system_apps.insert("com.legacy.app".to_string());
        e.refresh_system_apps();
        // 测试环境无 pm → 走失败分支 → system_apps 清为空集（回落语义）
        assert!(e.system_apps.is_empty(), "pm 失败回落空集");
        // 编译期名单独立兜底（不受 pm 影响）
        assert!(e.policy.is_critical("com.android.systemui"));
    }
}