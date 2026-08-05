//! L3 策略模型：conf/policy.toml 的解析与校验（docs/l3-plan.md §0.2/§0.3）。
//!
//! 铁律：解析失败 → 调用方保留旧策略表（失败安全）。未知键/段 → 警告不致命（前向兼容）。

use crate::toml::{parse, TomlEntry, TomlValue};
use crate::{logw, paths};
use std::collections::HashMap;

/// per-app 策略模式（[apps."pkg"] mode=...，参考 Cerberus 四策略分级）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    /// 豁免：退后台永不冻结（等价 whitelist，但允许携带 per-app 豁免开关）
    Exempt,
    /// 重要（v0.4.41-l3，对齐 AStop IMPORTANT 档）：可冻结但更温和——
    /// grace = 全局 ×2（退后台不急着冻），唤醒解冻强制开启（keep_wakeup 不可关）。
    /// 语义：微信/IM/网盘类"重要但不豁免"，墓碑而非杀。
    Important,
    /// 标准：按全局 grace_seconds（可被 grace_override 覆盖）
    Standard,
    /// 严格：失焦后短 grace 冻结（默认 8s，可被 grace_override 覆盖）
    Strict,
}

impl AppMode {
    fn parse(s: &str) -> Option<AppMode> {
        match s {
            "exempt" => Some(AppMode::Exempt),
            "important" => Some(AppMode::Important),
            "standard" => Some(AppMode::Standard),
            "strict" => Some(AppMode::Strict),
            _ => None,
        }
    }
}

impl Default for AppMode {
    /// 未知/缺省档回落 standard（与 parse 失败语义一致）
    fn default() -> Self {
        AppMode::Standard
    }
}

/// 子进程策略（v0.4.19-l3，参考 Cerberus：微信 :push / QQ MSF 必须处理）：
/// Keep = 冻结时保留 :push 类子进程（推送通道保持连接，防断网）
/// Kill = 冻结时连带杀死 :push 类子进程（通讯类 app 彻底休眠）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushMode {
    Keep,
    Kill,
}

impl PushMode {
    pub fn parse(s: &str) -> Option<PushMode> {
        match s {
            "keep" => Some(PushMode::Keep),
            "kill" => Some(PushMode::Kill),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            PushMode::Keep => "keep",
            PushMode::Kill => "kill",
        }
    }
}

/// per-app 策略条目（[apps."com.xxx"] 段）
#[derive(Debug, Clone)]
pub struct AppPolicy {
    pub mode: AppMode,
    /// grace 覆盖秒数（None = 跟随模式默认/全局）
    pub grace_override: Option<u64>,
    /// 前台服务豁免开关（None = 跟随全局 keep_fg_service）
    pub keep_fg_service: Option<bool>,
    /// 媒体播放豁免开关（None = 跟随全局 keep_media）
    pub keep_media: Option<bool>,
    /// 定位活动豁免开关（None = 跟随全局 keep_location；dex 侧 AppOps 判定 loc=1）
    pub keep_location: Option<bool>,
    /// 高网络负载豁免开关（None = 跟随全局 keep_high_network）
    pub keep_high_network: Option<bool>,
    /// 网络豁免开关（v0.4.23-l3，对齐 AStop force_network_exemption）：
    /// 有网络活动即不冻结（任何流量增量 >0，区别于 keep_high_network 的高阈值）；
    /// 已冻结时检测到网络活动 → 唤醒解冻（对齐 AStop allow_network_wakeup）。
    /// None = 跟随全局 keep_network
    pub keep_network: Option<bool>,
    /// 交互/FCM 唤醒豁免开关（None = 缺省 true：wakeup 事件照常解冻）
    pub keep_wakeup: Option<bool>,
    /// 子进程策略（None = 跟随全局 push_policy）
    pub push_mode: Option<PushMode>,
    /// 定时解冻窗口（每天 HH:MM-HH:MM，分钟数 0..=1439；None = 无窗口）
    pub unfreeze_window: Option<(u32, u32)>,
}

impl Default for AppPolicy {
    fn default() -> Self {
        Self {
            mode: AppMode::Standard,
            grace_override: None,
            keep_fg_service: None,
            keep_media: None,
            keep_location: None,
            keep_high_network: None,
            keep_network: None,
            keep_wakeup: None,
            push_mode: None,
            unfreeze_window: None,
        }
    }
}

impl AppPolicy {
    /// strict 模式缺省 grace（参考 Cerberus 严格档 5~8s）
    pub const STRICT_DEFAULT_GRACE: u64 = 8;
    /// important 模式 grace 倍数（全局 ×2：退后台不急着冻，给"重要但非豁免"app 更宽窗口）
    pub const IMPORTANT_GRACE_MULT: u64 = 2;

    /// 该 app 生效的 grace 秒数（strict 缺省 8；important = 全局 ×2；其余缺省回落全局）
    pub fn effective_grace(&self, global: u64) -> u64 {
        match self.grace_override {
            Some(g) => g,
            None => match self.mode {
                AppMode::Strict => Self::STRICT_DEFAULT_GRACE,
                AppMode::Important => global.saturating_mul(Self::IMPORTANT_GRACE_MULT),
                _ => global,
            },
        }
    }
}

/// 内置关键包清单（v0.4.24-l3，对齐 AStop critical_apps.txt 防呆哲学）。
///
/// 命中者**任何情况下不可冻结**——launcher/系统UI/电话/设置/输入法/VPN 授权/
/// 权限控制器被冻 = 系统故障、无法输入、全网断网（v0.4.22-l3 实机教训：VPN 被冻
/// → 全网断网 13 分钟）。该清单是内核级最低保护：
///   - 编译期内置，policy.toml 无法增删（防呆：UI/配置文件都改不掉）；
///   - 优先级最高：白名单/force/per-app/豁免链之前检查（含 freeze_now 最终防线）；
///   - 热更新对账：命中者已冻结 → 立即解冻。
pub const CRITICAL_PACKAGES: &[&str] = &[
    // launcher 族（切桌面黑屏 / 无法返回桌面）
    "com.android.launcher3",
    "com.android.launcher",
    "com.oplus.launcher",
    "com.coloros.launcher",
    "com.android.launcher2",
    // 系统关键 UI / 框架（冻结 = 状态栏/通知/设置/拨号全挂）
    "com.android.systemui",
    "com.android.settings",
    "com.android.phone",
    "com.android.shell",
    "com.android.permissioncontroller",
    // 输入法（冻结 = 无法打字；覆盖 AOSP/OPPO/百度/豆包/搜狗/讯飞）
    "com.android.inputmethod.latin",
    "com.oplus.inputmethod",
    "com.baidu.input_oppo",
    "com.baidu.input",
    "com.sohu.inputmethod.sogou",
    "com.iflytek.inputmethod",
    "com.bytedance.android.doubaoime",
    // VPN 授权对话框（AStop critical_apps.txt 实证：UI 层防呆也改不掉）
    "com.android.vpndialogs",
    // 系统通信（短信/存储提供者——冻结影响系统功能）
    "com.android.mms",
    "com.android.providers.media",
    // v0.4.49-l3 系统组件保护（2026-08-05 相机黑屏事故根因段）：
    // intentresolver/credentialmanager/packageinstaller 被 Sundown 当普通 app 冻结后，
    // 第三方 app 调用相机（隐式 Intent）→ IntentResolver 冻结 → freeze_binder 挂起 →
    // 长时间黑屏且"清后台重开无效"（残留冻结在系统组件层）。同类风险：文件选择/凭据/
    // 安装/相册/账号/密码本——隐式 Intent 与系统服务链路组件一律不可冻结。
    "com.android.intentresolver", // Intent 解析器（隐式 Intent/分享/调相机必经）
    "com.android.credentialmanager", // 凭据管理器（登录/密钥/权限确认链路）
    "com.android.packageinstaller", // 包安装器（安装/卸载/授权链路）
    "com.android.documentsui", // 文件选择器（隐式 Intent OPEN_DOCUMENT）
    "com.android.printspooler", // 打印服务（隐式 Intent 打印链路）
    "com.android.contacts", // 联系人（拨号/分享/账户链路）
    "com.android.providers.contacts", // 联系人存储
    "android.process.media", // 媒体处理（媒体扫描/存储链路）
    "com.android.providers.media.module", // 媒体存储模块（Android 13+）
    "com.coloros.gallery3d", // 相册（相机预览回显/分享联动）
    "com.oplus.camera", // 相机（启动链路敏感，防冻结竞态黑屏）
    "com.heytap.openid", // HeyTap 账号（登录/云服务链路）
    "com.coloros.codebook", // 密码本（密码填充/登录链路）
    "com.android.deskclock", // 闹钟（AOSP）
    "com.coloros.alarmclock", // 闹钟（ColorOS，系统 AppFreezer 曾冻结 ×2）
    "com.coloros.calendar", // 日历（ColorOS）
    "com.android.calendar", // 日历（AOSP）
];

/// 策略默认值（policy.toml 缺失/解析失败时的兜底：策略关闭，观测优先）
#[derive(Debug, Clone)]
pub struct Policy {
    /// 总开关（false = 只观测，不冻结）
    pub enabled: bool,
    /// 退后台 grace 秒数（防抖动）
    pub grace_seconds: u64,
    /// 解冻后冷却秒数（防"解冻-立即再冻"抖动）
    pub cooldown_seconds: u64,
    /// 唤醒节流秒数（v0.4.42-l3，对齐 AStop Probe 60s 限流）：
    /// 后台唤醒（broadcast/service/pendingintent）触发解冻后，窗口内同包再次唤醒不再解冻
    /// （防 FCM/广播风暴反复"解冻-再冻"抖动）；0 = 关闭节流；用户交互（focus）不受限
    pub wake_throttle_seconds: u64,
    /// 广播门控白名单（v0.4.43-l3，对齐 AStop Receiver gate 裁剪版）：
    /// 非空时，冻结 app 仅白名单广播 action 触发解冻（其余留痕 receiver_gated 不解冻）；
    /// 空 = 全部放行（保持既有行为，零风险默认）。仅约束 broadcast 源；
    /// service/pendingintent 唤醒不受门控；IMPORTANT 档 app 不受门控（保持"重要"语义）
    pub receiver_gate: Vec<String>,
    /// 强制冻结名单（命中即冻，优先级高于豁免动作，但白名单仍优先）
    pub force: Vec<String>,
    /// 永不冻结白名单
    pub whitelist: Vec<String>,
    /// per-app 策略表（[apps."pkg"]；缺失回落全局规则）
    pub apps: HashMap<String, AppPolicy>,
    /// 豁免动作：前台服务持有者不冻（dex 侧判定字段 fg=1）
    pub keep_fg_service: bool,
    /// 豁免动作：媒体播放持有者不冻（dex 侧判定字段 media=1）
    pub keep_media: bool,
    /// 豁免动作：定位使用中不冻（dex 侧 AppOps 判定字段 loc=1，v0.4.20-l3）
    pub keep_location: bool,
    /// 豁免动作：高网络负载不冻（daemon 侧流量采样判定，/proc/uid_stat）
    pub keep_high_network: bool,
    /// 豁免动作：网络豁免（v0.4.23-l3，对齐 AStop force_network_exemption）——
    /// 有网络活动（任何流量增量）即不冻结；已冻结时网络活动触发唤醒解冻
    pub keep_network: bool,
    /// 子进程策略（冻结时 :push 类子进程 保留/杀死，缺省 keep）
    pub push_policy: PushMode,
    /// VPN 守护进程保护（v0.4.22-l3）：true = 自动探测的 tun 持有者 + 手动列表永不冻结（缺省 true）
    pub keep_vpn: bool,
    /// VPN 手动兜底列表（自动探测失效时；命中即受保护，优先级最高）
    pub vpn_packages: Vec<String>,
    /// 防御 hook 组（L3 仅解析+展示，不启用）
    pub defense_anr: bool,
    pub defense_cached_optimizer: bool,
    /// [discard] 冻结超时丢弃秒数（v0.4.52-l3，行为概念《超时丢弃》）：
    /// 冻结集条目冻结时长超过该值且期间无任何唤醒命中（节流/门控拦截不算活跃）
    /// → 升级为丢弃（SIGKILL 整 uid，释放内存）；0 = 关闭（默认 1800 = 30min）
    pub discard_frozen_timeout_seconds: u64,
    /// [discard] 内存水位丢弃阈值 MB（v0.4.52-l3）：/proc/meminfo MemAvailable
    /// 低于该值 → 按 LRU（frozen_since 最旧优先）+ RSS 占用排序，丢弃冻结集
    /// 直到水位恢复；只作用于 Sundown 冻结集（白名单/IMPORTANT/critical/VPN/
    /// 系统组件/前台豁免天然不参与）；0 = 关闭（默认 512MB）
    pub discard_mem_watermark_mb: u64,
    /// [discard] 开机缓存回收（v0.4.52-l3）：boot_completed 后延迟
    /// boot_reclaim_delay_seconds 秒，扫描"上次会话 Sundown 冻结集 + 当前冻结集"
    /// 中 oom_score_adj ≥ 缓存档（900）的包 → 丢弃（"开机时的高缓存"主动回收）；
    /// 只回收 cache/empty 档，绝不动前台/感知/服务进程；true = 开启（默认）
    pub discard_boot_reclaim: bool,
    /// [discard] 开机回收延迟秒数（等系统恢复期结束；默认 120s）
    pub discard_boot_reclaim_delay_seconds: u64,
    /// 策略文件修订号（mtime 秒；热加载识别用）
    pub revision: u64,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            enabled: false,
            grace_seconds: 30,
            cooldown_seconds: 60,
            wake_throttle_seconds: 60,
            receiver_gate: Vec::new(),
            force: Vec::new(),
            whitelist: Vec::new(),
            apps: HashMap::new(),
            keep_fg_service: true,
            keep_media: true,
            keep_location: true,
            keep_high_network: true,
            keep_network: true,
            push_policy: PushMode::Keep,
            keep_vpn: true,
            vpn_packages: Vec::new(),
            defense_anr: false,
            defense_cached_optimizer: false,
            // v0.4.52-l3 超时丢弃：发布默认全部开启（直击"冻结≠释放内存"痛点），
            // 但受 [general] enabled 总开关约束——观望模式零动作
            discard_frozen_timeout_seconds: 1800,
            discard_mem_watermark_mb: 512,
            discard_boot_reclaim: true,
            discard_boot_reclaim_delay_seconds: 120,
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

    /// 内置关键包判定（v0.4.24-l3，对齐 AStop critical_apps.txt）——
    /// 编译期内置清单命中 = 任何情况下不可冻结（优先级最高，配置文件不可覆盖）
    pub fn is_critical(&self, pkg: &str) -> bool {
        CRITICAL_PACKAGES.iter().any(|c| *c == pkg)
    }

    /// 强制冻结名单判定
    pub fn is_forced(&self, pkg: &str) -> bool {
        self.force.iter().any(|f| f == pkg)
    }

    /// VPN 手动兜底列表判定（v0.4.22-l3）
    pub fn is_vpn_listed(&self, pkg: &str) -> bool {
        self.vpn_packages.iter().any(|v| v == pkg)
    }
}

fn apply_entry(p: &mut Policy, e: &TomlEntry) {
    let section = e.table.join(".");
    let val = &e.value;
    match (section.as_str(), e.key.as_str()) {
        ("general", "enabled") => p.enabled = bool_of(val, e, false),
        ("general", "grace_seconds") => p.grace_seconds = int_of(val, e, 30).max(0) as u64,
        ("general", "cooldown_seconds") => p.cooldown_seconds = int_of(val, e, 60).max(0) as u64,
        ("general", "wake_throttle_seconds") => p.wake_throttle_seconds = int_of(val, e, 60).max(0) as u64,
        ("freeze", "force") => p.force = str_array_of(val, e),
        ("whitelist", "packages") => p.whitelist = str_array_of(val, e),
        ("whitelist", "receiver_gate") => p.receiver_gate = str_array_of(val, e),
        ("whitelist", "keep_fg_service") => p.keep_fg_service = bool_of(val, e, true),
        ("whitelist", "keep_media") => p.keep_media = bool_of(val, e, true),
        ("whitelist", "keep_location") => p.keep_location = bool_of(val, e, true),
        ("whitelist", "keep_high_network") => p.keep_high_network = bool_of(val, e, true),
        ("whitelist", "keep_network") => p.keep_network = bool_of(val, e, true),
        ("whitelist", "push_policy") => match str_of(val, e) {
            Some(s) => match PushMode::parse(&s) {
                Some(m) => p.push_policy = m,
                None => logw!("策略 push_policy 未知（忽略，用 keep）: {}", s),
            },
            None => logw!("策略 push_policy 类型错误（忽略，用 keep）"),
        },
        ("defense", "anr_protect") => p.defense_anr = bool_of(val, e, false),
        ("defense", "cached_app_optimizer") => p.defense_cached_optimizer = bool_of(val, e, false),
        ("vpn", "keep_vpn") => p.keep_vpn = bool_of(val, e, true),
        ("vpn", "packages") => p.vpn_packages = str_array_of(val, e),
        // v0.4.52-l3：超时丢弃段（0=关闭，失败安全回落默认）
        ("discard", "frozen_timeout_seconds") => p.discard_frozen_timeout_seconds = int_of(val, e, 1800).max(0) as u64,
        ("discard", "mem_watermark_mb") => p.discard_mem_watermark_mb = int_of(val, e, 512).max(0) as u64,
        ("discard", "boot_reclaim") => p.discard_boot_reclaim = bool_of(val, e, true),
        ("discard", "boot_reclaim_delay_seconds") => p.discard_boot_reclaim_delay_seconds = int_of(val, e, 120).max(0) as u64,
        (s, _k) if s.starts_with("apps.") => apply_app_entry(p, &s[5..], e),
        (s, k) => {
            if !s.is_empty() {
                logw!("策略未知键（忽略）: [{}] {} = {}", s, k, debug_val(val));
            } else {
                logw!("策略未知顶层键（忽略）: {} = {}", k, debug_val(val));
            }
        }
    }
}

/// per-app 策略段解析：[apps."pkg"] mode/grace_seconds/keep_fg_service/keep_media
/// 未知键 → 警告不致命（前向兼容）；空 pkg（[apps] 裸段）→ 忽略
fn apply_app_entry(p: &mut Policy, pkg: &str, e: &TomlEntry) {
    if pkg.is_empty() {
        logw!("策略 [apps] 裸段（缺包名）忽略: {} = {}", e.key, debug_val(&e.value));
        return;
    }
    let ap = p.apps.entry(pkg.to_string()).or_insert_with(AppPolicy::default);
    let val = &e.value;
    match e.key.as_str() {
        "mode" => match str_of(val, e) {
            Some(s) => match AppMode::parse(&s) {
                Some(m) => ap.mode = m,
                None => logw!("策略 [apps.{}] mode 未知（忽略，用 standard）: {}", pkg, s),
            },
            None => logw!("策略 [apps.{}] mode 类型错误（忽略）", pkg),
        },
        "grace_seconds" => ap.grace_override = Some(int_of(val, e, 8).max(0) as u64),
        "keep_fg_service" => ap.keep_fg_service = Some(bool_of(val, e, true)),
        "keep_media" => ap.keep_media = Some(bool_of(val, e, true)),
        "keep_location" => ap.keep_location = Some(bool_of(val, e, true)),
        "keep_high_network" => ap.keep_high_network = Some(bool_of(val, e, true)),
        "keep_network" => ap.keep_network = Some(bool_of(val, e, true)),
        "keep_wakeup" => ap.keep_wakeup = Some(bool_of(val, e, true)),
        "push_mode" => match str_of(val, e) {
            Some(s) => match PushMode::parse(&s) {
                Some(m) => ap.push_mode = Some(m),
                None => logw!("策略 [apps.{}] push_mode 未知（忽略，跟随全局）: {}", pkg, s),
            },
            None => logw!("策略 [apps.{}] push_mode 类型错误（忽略，跟随全局）", pkg),
        },
        "unfreeze_window" => match str_of(val, e) {
            Some(s) => match parse_window(&s) {
                Some(w) => ap.unfreeze_window = Some(w),
                None => logw!("策略 [apps.{}] unfreeze_window 格式错误（应为 HH:MM-HH:MM，忽略）: {}", pkg, s),
            },
            None => logw!("策略 [apps.{}] unfreeze_window 类型错误（忽略）", pkg),
        },
        k => logw!("策略 [apps.{}] 未知键（忽略）: {} = {}", pkg, k, debug_val(val)),
    }
}

fn str_of(v: &TomlValue, e: &TomlEntry) -> Option<String> {
    match v {
        TomlValue::Str(s) => Some(s.clone()),
        _ => {
            logw!("策略键类型错误（期望字符串）: {} = {}", e.key, debug_val(v));
            None
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

/// 解析定时窗口 "HH:MM-HH:MM" → (开始分钟, 结束分钟)，0..=1439，start <= end。
/// 格式错误 / 跨零点（start > end）→ None（不支持跨零点，文档说明）。
fn parse_window(s: &str) -> Option<(u32, u32)> {
    let (a, b) = s.split_once('-')?;
    let start = parse_hhmm(a.trim())?;
    let end = parse_hhmm(b.trim())?;
    if start > end {
        return None;
    }
    Some((start, end))
}

fn parse_hhmm(s: &str) -> Option<u32> {
    let (h, m) = s.split_once(':')?;
    let h: u32 = h.trim().parse().ok()?;
    let m: u32 = m.trim().parse().ok()?;
    if h > 23 || m > 59 {
        return None;
    }
    Some(h * 60 + m)
}

/// 分钟数 minute（0..=1439）是否落在 (start, end) 窗口内（含边界；None = 无窗口恒 false）。
/// 纯函数——单测直接覆盖，引擎侧经本地时间调用。
pub fn in_window(minute: u32, window: Option<(u32, u32)>) -> bool {
    match window {
        Some((s, e)) => minute >= s && minute <= e,
        None => false,
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

    /// v0.4.52-l3：[discard] 段解析——超时/水位/开机回收四参数 + 缺省回落 + 0=关闭
    #[test]
    fn discard_parse_v052() {
        let src = r#"
[general]
enabled = true

[discard]
frozen_timeout_seconds = 3600
mem_watermark_mb = 256
boot_reclaim = false
boot_reclaim_delay_seconds = 60
"#;
        let p = Policy::from_toml(src, 99).unwrap();
        assert!(p.enabled);
        assert_eq!(p.discard_frozen_timeout_seconds, 3600);
        assert_eq!(p.discard_mem_watermark_mb, 256);
        assert!(!p.discard_boot_reclaim);
        assert_eq!(p.discard_boot_reclaim_delay_seconds, 60);

        // 缺省回落（发布默认全部开启，受 enabled 总开关约束）
        let d = Policy::default();
        assert_eq!(d.discard_frozen_timeout_seconds, 1800);
        assert_eq!(d.discard_mem_watermark_mb, 512);
        assert!(d.discard_boot_reclaim);
        assert_eq!(d.discard_boot_reclaim_delay_seconds, 120);

        // 0 = 关闭（钳 0 不解析为负）
        let src2 = "[discard]\nfrozen_timeout_seconds = 0\nmem_watermark_mb = 0";
        let p2 = Policy::from_toml(src2, 1).unwrap();
        assert_eq!(p2.discard_frozen_timeout_seconds, 0);
        assert_eq!(p2.discard_mem_watermark_mb, 0);
        assert!(p2.discard_boot_reclaim, "boot_reclaim 未指定回落默认 true");
    }

    #[test]
    fn parse_per_app_policy() {
        let src = r#"
[general]
enabled = true
grace_seconds = 30

[apps."com.tencent.mm"]
mode = "strict"
grace_seconds = 8
keep_media = false

[apps."com.example.tool"]
mode = "exempt"

[apps."com.example.normal"]
mode = "standard"
grace_seconds = 120
"#;
        let p = Policy::from_toml(src, 7).unwrap();
        assert!(p.enabled);
        assert_eq!(p.apps.len(), 3);

        // strict：短 grace + per-app 豁免开关覆盖
        let mm = p.apps.get("com.tencent.mm").unwrap();
        assert_eq!(mm.mode, AppMode::Strict);
        assert_eq!(mm.effective_grace(30), 8);
        assert_eq!(mm.keep_media, Some(false));
        assert_eq!(mm.keep_fg_service, None);

        // exempt：永不冻结
        let tool = p.apps.get("com.example.tool").unwrap();
        assert_eq!(tool.mode, AppMode::Exempt);
        assert_eq!(tool.effective_grace(30), 30); // exempt 不因 strict 走 8s

        // standard：grace 覆盖生效，未覆盖回落全局
        let normal = p.apps.get("com.example.normal").unwrap();
        assert_eq!(normal.mode, AppMode::Standard);
        assert_eq!(normal.effective_grace(30), 120);
        assert!(p.apps.get("com.example.missing").is_none());

        // strict 缺省 grace（无 override）
        let src2 = "[apps.\"com.x.y\"]\nmode = \"strict\"";
        let p2 = Policy::from_toml(src2, 8).unwrap();
        assert_eq!(p2.apps["com.x.y"].effective_grace(30), AppPolicy::STRICT_DEFAULT_GRACE);
    }
    #[test]
    fn parse_per_app_bad_mode() {
        // 未知 mode → 回落 standard（不致命）
    }

    #[test]
    fn important_mode_v041() {
        // v0.4.41-l3：IMPORTANT 档（对齐 AStop）——grace = 全局 ×2，解析 + 缺省回落
        let src = "[general]\nenabled = true\ngrace_seconds = 30\n\n[apps.\"com.tencent.mm\"]\nmode = \"important\"\nkeep_wakeup = false";
        let p = Policy::from_toml(src, 1).unwrap();
        let ap = p.apps.get("com.tencent.mm").unwrap();
        assert_eq!(ap.mode, AppMode::Important);
        // grace = 全局 ×2
        assert_eq!(ap.effective_grace(30), 60);
        // 无 override 时 important 缺省 = 全局 ×2（与 strict 8s 区分）
        let src2 = "[apps.\"com.example.imp\"]\nmode = \"important\"";
        let p2 = Policy::from_toml(src2, 1).unwrap();
        assert_eq!(p2.apps["com.example.imp"].effective_grace(45), 90);
        // grace_override 优先于档位缺省
        let src3 = "[apps.\"com.example.imp2\"]\nmode = \"important\"\ngrace_seconds = 15";
        let p3 = Policy::from_toml(src3, 1).unwrap();
        assert_eq!(p3.apps["com.example.imp2"].effective_grace(30), 15);
        // 未知档仍回落 standard
        assert_eq!(AppMode::parse("turbo"), None);
    }

    #[test]
    fn wake_throttle_parse_v042() {
        // v0.4.42-l3：wake_throttle_seconds 解析（缺省 60 / 显式覆盖 / 0=关闭）
        let p = Policy::from_toml("[general]\nenabled = true", 1).unwrap();
        assert_eq!(p.wake_throttle_seconds, 60); // 缺省 60（对齐 AStop Probe 限流）
        let p2 = Policy::from_toml("[general]\nwake_throttle_seconds = 120", 2).unwrap();
        assert_eq!(p2.wake_throttle_seconds, 120);
        let p3 = Policy::from_toml("[general]\nwake_throttle_seconds = 0", 3).unwrap();
        assert_eq!(p3.wake_throttle_seconds, 0); // 0 = 关闭节流
        // 坏值回落缺省（失败安全）
        let p4 = Policy::from_toml("[general]\nwake_throttle_seconds = -5", 4).unwrap();
        assert_eq!(p4.wake_throttle_seconds, 0); // 负数钳 0
    }

    #[test]
    fn receiver_gate_parse_v043() {
        // v0.4.43-l3：receiver_gate 广播门控白名单解析（缺省空 = 全放行）
        let p = Policy::from_toml("[general]\nenabled = true", 1).unwrap();
        assert!(p.receiver_gate.is_empty());
        let src = "[whitelist]\nreceiver_gate = [\"android.intent.action.USER_PRESENT\", \"android.intent.action.BOOT_COMPLETED\"]";
        let p2 = Policy::from_toml(src, 2).unwrap();
        assert_eq!(
            p2.receiver_gate,
            vec![
                "android.intent.action.USER_PRESENT".to_string(),
                "android.intent.action.BOOT_COMPLETED".to_string()
            ]
        );
    }

    #[test]
    fn parse_exempt_dimensions_v0419() {
        // v0.4.19-l3：新增豁免维度解析（全局 + per-app）
        let src = r#"
[whitelist]
keep_high_network = false
push_policy = "kill"

[apps."com.tencent.mm"]
keep_high_network = true
keep_wakeup = false
push_mode = "keep"
unfreeze_window = "21:00-08:00"

[apps."com.example.bad"]
push_mode = "turbo"
unfreeze_window = "25:00-26:00"
keep_wakeup = 1

[apps."com.example.night"]
unfreeze_window = "22:30-23:45"
"#;
        let p = Policy::from_toml(src, 9).unwrap();
        // 全局
        assert!(!p.keep_high_network);
        assert_eq!(p.push_policy, PushMode::Kill);
        // per-app 覆盖
        let mm = p.apps.get("com.tencent.mm").unwrap();
        assert_eq!(mm.keep_high_network, Some(true));
        assert_eq!(mm.keep_wakeup, Some(false));
        assert_eq!(mm.push_mode, Some(PushMode::Keep));
        assert_eq!(mm.unfreeze_window, None); // 21:00-08:00 跨零点 → 拒绝（不支持跨零点）
        // 坏值：push_mode 未知 → None（跟随全局 kill）；unfreeze_window 非法 → None；keep_wakeup 类型错 → 回落默认 true
        let bad = p.apps.get("com.example.bad").unwrap();
        assert_eq!(bad.push_mode, None);
        assert_eq!(bad.unfreeze_window, None);
        assert_eq!(bad.keep_wakeup, Some(true)); // bool_of 类型错误回落默认值（失败安全）
        // 合法窗口
        let night = p.apps.get("com.example.night").unwrap();
        assert_eq!(night.unfreeze_window, Some((22 * 60 + 30, 23 * 60 + 45)));
    }

    #[test]
    fn parse_location_dimension_v0420() {
        // v0.4.20-l3：定位活动豁免维度（全局 + per-app）
        let src = r#"
[whitelist]
keep_location = false

[apps."com.example.navi"]
keep_location = true

[apps."com.example.bad"]
keep_location = 1
"#;
        let p = Policy::from_toml(src, 10).unwrap();
        assert!(!p.keep_location); // 全局关闭
        assert_eq!(p.apps["com.example.navi"].keep_location, Some(true)); // per-app 覆盖
        assert_eq!(p.apps["com.example.bad"].keep_location, Some(true)); // 类型错误回落默认 true（失败安全）
        // 缺省（未配置）回落全局语义由引擎 keep_loc 方法处理：None → 全局
        assert_eq!(Policy::default().keep_location, true);
    }

    #[test]
    fn parse_window_ok_and_bad() {
        // 合法
        assert_eq!(parse_window("09:00-22:00"), Some((540, 1320)));
        assert_eq!(parse_window("0:00-23:59"), Some((0, 1439)));
        assert_eq!(parse_window("22:30-23:45"), Some((1350, 1425)));
        // 非法：跨零点 / 时间越界 / 缺冒号 / 非数字
        assert_eq!(parse_window("21:00-08:00"), None);
        assert_eq!(parse_window("24:00-08:00"), None);
        assert_eq!(parse_window("09-22"), None);
        assert_eq!(parse_window("ab:cd-ef:gh"), None);
        assert_eq!(parse_window(""), None);
    }

    #[test]
    fn in_window_bounds() {
        // 含边界；无窗口恒 false
        assert!(in_window(540, Some((540, 1320))));
        assert!(in_window(1320, Some((540, 1320))));
        assert!(in_window(900, Some((540, 1320))));
        assert!(!in_window(539, Some((540, 1320))));
        assert!(!in_window(1321, Some((540, 1320))));
        assert!(!in_window(900, None));
    }

    #[test]
    fn push_mode_parse() {
        assert_eq!(PushMode::parse("keep"), Some(PushMode::Keep));
        assert_eq!(PushMode::parse("kill"), Some(PushMode::Kill));
        assert_eq!(PushMode::parse("nuke"), None);
        assert_eq!(PushMode::Keep.as_str(), "keep");
        assert_eq!(PushMode::Kill.as_str(), "kill");
    }

    #[test]
    fn parse_vpn_section_v0422() {
        // v0.4.22-l3：VPN 守护进程保护段（keep_vpn + 手动兜底列表）
        let src = r#"
[vpn]
keep_vpn = true
packages = [ "com.example.clash", "com.example.v2ray" ]
"#;
        let p = Policy::from_toml(src, 11).unwrap();
        assert!(p.keep_vpn);
        assert!(p.is_vpn_listed("com.example.clash"));
        assert!(p.is_vpn_listed("com.example.v2ray"));
        assert!(!p.is_vpn_listed("com.example.other"));
        // 缺省：keep_vpn = true（安全优先）
        assert!(Policy::default().keep_vpn);
        assert!(Policy::default().vpn_packages.is_empty());
        // keep_vpn = false 显式关闭
        let p2 = Policy::from_toml("[vpn]\nkeep_vpn = false", 12).unwrap();
        assert!(!p2.keep_vpn);
        // 类型错误回落默认 true
        let p3 = Policy::from_toml("[vpn]\nkeep_vpn = 1", 13).unwrap();
        assert!(p3.keep_vpn);
    }

    #[test]
    fn parse_network_dimension_v0423() {
        // v0.4.23-l3：网络豁免维度（全局 + per-app，对齐 AStop force_network_exemption）
        let src = r#"
[whitelist]
keep_network = false

[apps."com.example.downloader"]
keep_network = true

[apps."com.example.bad"]
keep_network = 1
"#;
        let p = Policy::from_toml(src, 14).unwrap();
        assert!(!p.keep_network); // 全局关闭
        assert_eq!(p.apps["com.example.downloader"].keep_network, Some(true)); // per-app 覆盖
        assert_eq!(p.apps["com.example.bad"].keep_network, Some(true)); // 类型错误回落默认 true（失败安全）
        // 缺省（未配置）回落全局语义由引擎 keep_net 方法处理：None → 全局
        assert_eq!(Policy::default().keep_network, true);
        // 默认 per-app None（跟随全局）
        assert!(Policy::default().apps.get("x").is_none());
        let p2 = Policy::from_toml("[apps.\"com.x.y\"]\nmode = \"strict\"", 15).unwrap();
        assert_eq!(p2.apps["com.x.y"].keep_network, None);
    }

    #[test]
    fn critical_list_v0424() {
        // v0.4.24-l3：内置关键包清单——编译期常量判定，与配置无关（防呆核心）
        let p = Policy::default();
        // launcher / 系统 UI / 电话 / 设置 / 输入法 / VPN 授权 / 权限控制器
        for c in [
            "com.android.launcher3",
            "com.android.launcher",
            "com.oplus.launcher",
            "com.coloros.launcher",
            "com.android.systemui",
            "com.android.settings",
            "com.android.phone",
            "com.android.shell",
            "com.android.permissioncontroller",
            "com.android.inputmethod.latin",
            "com.baidu.input_oppo",
            "com.bytedance.android.doubaoime",
            "com.android.vpndialogs",
            "com.android.mms",
            "com.android.providers.media",
        ] {
            assert!(p.is_critical(c), "{} 应在 critical 清单", c);
        }
        // 普通包不受影响（即使配置里显式写 force 也不行——引擎层已先于 force 检查）
        assert!(!p.is_critical("com.example.normal"));
        assert!(!p.is_critical("com.tencent.mm"));
        // 配置无法覆盖：解析任意 policy 后 critical 判定不变
        let p2 = Policy::from_toml(
            "[freeze]\nforce = [\"com.android.systemui\"]\n[general]\nenabled = true",
            16,
        )
        .unwrap();
        assert!(p2.is_forced("com.android.systemui"));
        assert!(p2.is_critical("com.android.systemui")); // critical 仍命中（引擎检查顺序 critical > force）
        // 白名单语义独立
        let p3 = Policy::from_toml("[whitelist]\npackages = [\"com.tencent.mm\"]", 17).unwrap();
        assert!(p3.is_whitelisted("com.tencent.mm"));
        assert!(!p3.is_critical("com.tencent.mm"));
    }
}