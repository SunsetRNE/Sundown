//! 共享状态与 status JSON 构造。
//! 输出字段向后兼容 docs/sunctl-spec.md 的 status --json 契约（只增不改）。

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use crate::engine::EngineState;
use crate::{logi, logw, paths};

/// L1 探针桩的 hello-probe 上报记录
pub struct ProbeReport {
    pub build_hash: String,
    /// 距 daemon 启动的秒数（单调时钟，免系统时间跳变）
    pub reported_after_secs: u64,
}

/// L2 dex 层的 hello-dex 上报记录
pub struct DexReport {
    /// dex 构建版本（= CI 构建 commit short sha，与桩 hash 同源闭环）
    pub build_version: String,
    pub reported_after_secs: u64,
}

/// L2b native 伴生库的 report-bridge 上报记录
pub struct BridgeReport {
    /// bridge 构建 hash（= CI 构建 commit short sha，与桩/dex hash 同源闭环）
    pub build_hash: String,
    pub reported_after_secs: u64,
}

/// B2 事件订阅过滤器（v0.7-l3，缺口补入清单 B2 事件订阅注册表）。
/// 订阅者（dex 层）经 `subscribe` 命令声明兴趣，daemon 按需分发替代全量广播。
/// 设计纪律：
///   - 默认全量（Default = 收所有事件）——旧 dex 不声明 subscribe 行为不变（零风险兼容）
///   - 过滤维度：事件类型（kinds）+ 包名（packages）双轴；级别维度对下行事件不适用
///     （下行事件仅同步类：frozen-sync/candidate-sync/dex-push，无 EvLevel 概念，未来分级再扩展）
///   - 包名匹配：精确；`pkg.*` 结尾通配符前缀匹配（对齐 policy 通配直觉）
pub struct Subscription {
    /// 感兴趣的事件类型（下行 kind：frozen-sync / candidate-sync / dex-push）；
    /// 空 = 全量收（默认，兼容旧 dex）
    pub kinds: Vec<String>,
    /// 包名过滤（从事件行内 `pkg=` 提取；无 pkg= 的事件仅按 kinds 过滤）；
    /// 空 = 全部包（默认）
    pub packages: Vec<String>,
}

impl Default for Subscription {
    fn default() -> Self {
        Self {
            kinds: Vec::new(),
            packages: Vec::new(),
        }
    }
}

impl Subscription {
    /// 匹配判定（纯函数，供 broadcast 过滤与单元测试）：
    /// kind 不在兴趣集（kinds 非空时）→ 不匹配；事件带 pkg 且 packages 非空且不命中 → 不匹配。
    pub fn matches(&self, kind: &str, pkg: Option<&str>) -> bool {
        if !self.kinds.is_empty() && !self.kinds.iter().any(|k| k == kind) {
            return false;
        }
        if let Some(p) = pkg {
            if !self.packages.is_empty() {
                let hit = self.packages.iter().any(|pat| {
                    if let Some(prefix) = pat.strip_suffix(".*") {
                        p.starts_with(prefix) && (p.len() > prefix.len())
                    } else {
                        p == pat
                    }
                });
                if !hit {
                    return false;
                }
            }
        }
        true
    }

    /// 事件行内 `pkg=` 提取（无则 None；frozen-sync/candidate-sync 只有 uid= 不参与包名过滤）
    pub fn pkg_of(line: &str) -> Option<&str> {
        line.split_whitespace()
            .find_map(|t| t.strip_prefix("pkg="))
            .filter(|v| !v.is_empty())
    }
}

pub struct DaemonState {
    pub started_at: Instant,
    pub config_reloads: AtomicU64,
    pub connections_served: AtomicU64,
    /// 探针桩最近一次 hello-probe 上报（L1：只存最新一条即够）
    pub probe: Mutex<Option<ProbeReport>>,
    /// 期望的桩 build hash（模块内 probe.hash；模块未含桩或文件缺失时为 None）
    pub expected_probe_hash: Mutex<Option<String>>,
    /// dex 层最近一次 hello-dex 上报（L2：只存最新一条即够）
    pub dex: Mutex<Option<DexReport>>,
    /// 期望的 dex 构建版本（模块内 probe.dex.hash；缺失时为 None，dev 场景）
    pub expected_dex_hash: Mutex<Option<String>>,
    /// hello-dex 事件订阅连接注册表（push-dex 推送对象）；
    /// 元素为 (订阅 id, 可写副本, 订阅过滤器)，id 单调分配，断连/写失败剔除
    pub dex_clients: Mutex<Vec<(u64, UnixStream, Subscription)>>,
    pub next_dex_client_id: AtomicU64,
    /// bridge 最近一次 report-bridge 上报（L2b：只存最新一条即够）
    pub hook_bridge: Mutex<Option<BridgeReport>>,
    /// 期望的 bridge build hash（模块内 hook/hook.hash；缺失时为 None）
    pub expected_hook_hash: Mutex<Option<String>>,
    /// 最近一次焦点包名（event focus 上行；L2b 观测面）
    pub last_focus_pkg: Mutex<Option<String>>,
    /// 焦点切换累计次数
    pub focus_changes: AtomicU64,
    /// 唤醒入口命中累计次数（event wakeup 上行）
    pub wakeup_events: AtomicU64,
    /// L3 策略引擎（策略表 + 冻结表 + 决策状态机）
    pub engine: Mutex<EngineState>,
}

/// 从模块目录读取期望 hash（启动 / reload-config 时调用）
fn read_expected_hash() -> Option<String> {
    std::fs::read_to_string(paths::PROBE_EXPECTED_HASH_FILE)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 从模块目录读取期望 dex 构建版本（同上，L2 闭环）
fn read_expected_dex_hash() -> Option<String> {
    std::fs::read_to_string(paths::PROBE_EXPECTED_DEX_HASH_FILE)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 从模块目录读取期望 bridge build hash（同上，L2b 闭环）
fn read_expected_hook_hash() -> Option<String> {
    std::fs::read_to_string(paths::PROBE_EXPECTED_HOOK_HASH_FILE)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

impl DaemonState {
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
            config_reloads: AtomicU64::new(0),
            connections_served: AtomicU64::new(0),
            probe: Mutex::new(None),
            expected_probe_hash: Mutex::new(read_expected_hash()),
            dex: Mutex::new(None),
            expected_dex_hash: Mutex::new(read_expected_dex_hash()),
            dex_clients: Mutex::new(Vec::new()),
            next_dex_client_id: AtomicU64::new(0),
            hook_bridge: Mutex::new(None),
            expected_hook_hash: Mutex::new(read_expected_hook_hash()),
            last_focus_pkg: Mutex::new(None),
            focus_changes: AtomicU64::new(0),
            wakeup_events: AtomicU64::new(0),
            engine: Mutex::new(Self::init_engine()),
        }
    }

    /// 初始策略：读 conf/policy.toml，失败用默认（策略关闭，观测优先）；
    /// 情景预设表一并加载（v0.4.18-l3 修复：此前仅 reload 时刷新，重启后为空）
    fn init_engine() -> EngineState {
        let mut e = EngineState::default();
        // v0.4.49-l3：启动即枚举系统 app 保护清单（pm list packages -s；失败回落编译期名单）
        e.refresh_system_apps();
        if let Some((p, _)) = crate::policy::Policy::load() {
            logi!(
                "L3 策略已加载: enabled={} grace={}s cooldown={}s whitelist={} force={} apps={}（revision={}）",
                p.enabled,
                p.grace_seconds,
                p.cooldown_seconds,
                p.whitelist.len(),
                p.force.len(),
                p.apps.len(),
                p.revision
            );
            e.policy = p;
        } else {
            logw!("L3 初始策略缺失/解析失败（策略关闭，观测模式）: {}", paths::POLICY_FILE);
        }
        // L3 情景预设：启动即加载 action.toml（缺失/解析失败 → 空表，不致命）
        e.presets = crate::preset::PresetTable::load();
        let pn = e.presets.names();
        if !pn.is_empty() {
            logi!(
                "L3 情景预设已加载: {}（revision={}）",
                pn.join(", "),
                e.presets.revision
            );
        } else {
            logw!("L3 情景预设为空（action.toml 缺失或无预设）: {}", paths::ACTION_FILE);
        }
        e
    }

    pub fn uptime_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }

    pub fn bump_config_reloads(&self) {
        self.config_reloads.fetch_add(1, Ordering::Relaxed);
    }

    pub fn bump_connections(&self) {
        self.connections_served.fetch_add(1, Ordering::Relaxed);
    }

    /// hello-probe：记录桩上报的 build hash
    pub fn record_probe(&self, hash: &str) {
        let report = ProbeReport {
            build_hash: hash.to_string(),
            reported_after_secs: self.uptime_secs(),
        };
        *self.probe.lock().unwrap() = Some(report);
    }

    /// 当前期望 hash（None = 模块内无 probe.hash，例如本地 dev 构建）
    pub fn expected_hash(&self) -> Option<String> {
        self.expected_probe_hash.lock().unwrap().clone()
    }

    /// reload-config 时同步重读期望 hash（模块 zip 更新后生效）
    pub fn refresh_expected_hash(&self) {
        *self.expected_probe_hash.lock().unwrap() = read_expected_hash();
        *self.expected_dex_hash.lock().unwrap() = read_expected_dex_hash();
        *self.expected_hook_hash.lock().unwrap() = read_expected_hook_hash();
    }

    // ---------------- L2 dex 层状态 ----------------

    /// hello-dex：记录 dex 上报的构建版本
    pub fn record_dex(&self, version: &str) {
        let report = DexReport {
            build_version: version.to_string(),
            reported_after_secs: self.uptime_secs(),
        };
        *self.dex.lock().unwrap() = Some(report);
    }

    /// 当前期望的 dex 构建版本（None = 模块内无 probe.dex.hash）
    pub fn expected_dex_hash(&self) -> Option<String> {
        self.expected_dex_hash.lock().unwrap().clone()
    }

    // ---------------- L2b bridge / 事件观测面 ----------------

    /// report-bridge：记录 bridge 上报的 build hash
    pub fn record_bridge(&self, hash: &str) {
        let report = BridgeReport {
            build_hash: hash.to_string(),
            reported_after_secs: self.uptime_secs(),
        };
        *self.hook_bridge.lock().unwrap() = Some(report);
    }

    /// 当前期望的 bridge build hash（None = 模块内无 hook/hook.hash）
    pub fn expected_hook_hash(&self) -> Option<String> {
        self.expected_hook_hash.lock().unwrap().clone()
    }

    /// event focus：记录最新焦点包名并累计切换次数
    pub fn record_focus(&self, pkg: &str) {
        *self.last_focus_pkg.lock().unwrap() = Some(pkg.to_string());
        self.focus_changes.fetch_add(1, Ordering::Relaxed);
    }

    /// event wakeup：累计唤醒入口命中次数（广播风暴下只计数不逐条日志）
    pub fn bump_wakeup(&self) {
        self.wakeup_events.fetch_add(1, Ordering::Relaxed);
    }

    // ---------------- dex 事件订阅注册表 ----------------

    pub fn register_dex_client(&self, stream: UnixStream) -> u64 {
        let id = self.next_dex_client_id.fetch_add(1, Ordering::Relaxed) + 1;
        // B2：默认全量订阅（Subscription::default = 收所有事件，旧 dex 兼容零风险）
        self.dex_clients.lock().unwrap().push((id, stream, Subscription::default()));
        id
    }

    pub fn unregister_dex_client(&self, id: u64) {
        self.dex_clients.lock().unwrap().retain(|(i, _, _)| *i != id);
    }

    /// B2（v0.7-l3）：更新订阅过滤器（`subscribe` 命令入口）；
    /// 未知 id（连接已断）静默忽略。返回是否更新成功。
    pub fn update_dex_subscription(&self, id: u64, sub: Subscription) -> bool {
        let mut clients = self.dex_clients.lock().unwrap();
        match clients.iter_mut().find(|(i, _, _)| *i == id) {
            Some((_, _, s)) => {
                *s = sub;
                true
            }
            None => false,
        }
    }

    /// 当前订阅过滤器快照（`subscribe query` 应答用）；未知 id → None
    pub fn dex_subscription(&self, id: u64) -> Option<Subscription> {
        self.dex_clients
            .lock()
            .unwrap()
            .iter()
            .find(|(i, _, _)| *i == id)
            .map(|(_, _, s)| Subscription {
                kinds: s.kinds.clone(),
                packages: s.packages.clone(),
            })
    }

    /// push-dex 广播：向匹配订阅连接写事件头行 + dex 字节帧；
    /// B2：仅分发 kind=dex-push 且包名过滤命中的订阅者（dex-push 无 pkg=，实际只按 kind）。
    /// 写失败的连接视为断连剔除。返回成功通知的订阅者数量。
    pub fn broadcast_dex(&self, header_line: &[u8], payload: &[u8]) -> usize {
        let mut notified = 0usize;
        self.dex_clients
            .lock()
            .unwrap()
            .retain_mut(|(_, s, sub)| {
                if !sub.matches("dex-push", None) {
                    return true; // 不感兴趣：保留连接但不通知
                }
                let ok = s
                    .write_all(header_line)
                    .and_then(|_| s.write_all(payload))
                    .and_then(|_| s.flush())
                    .is_ok();
                if ok {
                    notified += 1;
                }
                ok
            });
        notified
    }

    /// v0.4.27-l3 行广播 → B2 按需分发：向匹配订阅连接写一行下行事件
    /// （frozen-sync / candidate-sync 等，无字节帧）；写失败连接剔除。返回成功数量。
    /// kind：事件类型（过滤维度一）；行内 pkg= 参与包名过滤（维度二，无则仅 kind）。
    pub fn broadcast_line(&self, kind: &str, line: &str) -> usize {
        let mut notified = 0usize;
        let bytes = line.as_bytes();
        let pkg = Subscription::pkg_of(line);
        self.dex_clients
            .lock()
            .unwrap()
            .retain_mut(|(_, s, sub)| {
                if !sub.matches(kind, pkg) {
                    return true; // 不感兴趣：保留连接但不通知
                }
                let ok = s.write_all(bytes).and_then(|_| s.flush()).is_ok();
                if ok {
                    notified += 1;
                }
                ok
            });
        notified
    }

    /// 兼容 sunctl-spec 的 status JSON（socket 应答版）。
    /// L1 起 probe_stub_loaded / probe_stub_build_hash 填真实值；
    /// L2 起 probe_dex_version 填真实值并新增 probe_dex_hash_match（契约只增不改）。
    pub fn status_json(&self) -> String {
        let (stub_loaded, stub_hash_json) = {
            let guard = self.probe.lock().unwrap();
            match guard.as_ref() {
                Some(p) => (1, format!("\"{}\"", p.build_hash)),
                None => (0, "null".to_string()),
            }
        };
        let (dex_version_json, dex_hash_match) = {
            let guard = self.dex.lock().unwrap();
            let expected = self.expected_dex_hash.lock().unwrap();
            match guard.as_ref() {
                Some(d) => {
                    let m = match expected.as_ref() {
                        Some(e) if e == &d.build_version => 1,
                        Some(_) => 0,
                        None => -1,
                    };
                    (format!("\"{}\"", d.build_version), m)
                }
                None => ("null".to_string(), -1),
            }
        };
        let (bridge_hash_json, bridge_hash_match) = {
            let guard = self.hook_bridge.lock().unwrap();
            let expected = self.expected_hook_hash.lock().unwrap();
            match guard.as_ref() {
                Some(b) => {
                    let m = match expected.as_ref() {
                        Some(e) if e == &b.build_hash => 1,
                        Some(_) => 0,
                        None => -1,
                    };
                    (format!("\"{}\"", b.build_hash), m)
                }
                None => ("null".to_string(), -1),
            }
        };
        let focus_pkg_json = match self.last_focus_pkg.lock().unwrap().as_ref() {
            Some(p) => format!("\"{}\"", p),
            None => "null".to_string(),
        };
        // L3 策略引擎快照（契约只增不改）
        let eng = self.engine.lock().unwrap();
        let frozen_json = json_str_array(&eng.frozen_packages());
        let grace_json = json_str_array(&eng.grace_pending());
        let (
            policy_enabled,
            policy_revision,
            policy_apps,
            events_count,
            events_total,
            freeze_ops,
            unfreeze_ops,
            wakeup_thaws,
            wake_throttled,
            discard_ops,
            discard_frozen_timeout,
            discard_mem_watermark,
            discard_boot_reclaim,
            discard_timeout_s,
        ) = (
            eng.policy.enabled,
            eng.policy.revision,
            eng.policy.apps.len(),
            eng.events.len(),
            eng.events.total,
            eng.freeze_ops,
            eng.unfreeze_ops,
            eng.wakeup_thaws,
            eng.wake_throttled,
            eng.discard_ops,
            eng.discard_frozen_timeout,
            eng.discard_mem_watermark,
            eng.discard_boot_reclaim,
            eng.policy.discard_frozen_timeout_seconds,
        );
        let discarded_json = json_str_array(&eng.discarded);
        drop(eng);
        format!(
            concat!(
                "{{",
                "\"module\":\"sundown\",",
                "\"version\":\"{ver}\",",
                "\"release_no\":{rel},",
                "\"daemon_running\":1,",
                "\"daemon_pid\":{pid},",
                "\"daemon_ready\":1,",
                "\"zygisk_provider\":null,",
                "\"probe_stub_loaded\":{stub_loaded},",
                "\"probe_stub_build_hash\":{stub_hash},",
                "\"probe_dex_version\":{dex_version},",
                "\"probe_dex_hash_match\":{dex_match},",
                "\"probe_hook_bridge_hash\":{bridge_hash},",
                "\"probe_hook_bridge_hash_match\":{bridge_match},",
                "\"focus_pkg\":{focus_pkg},",
                "\"focus_changes\":{focus_changes},",
                "\"wakeup_events\":{wakeup_events},",
                "\"policy_enabled\":{policy_enabled},",
                "\"policy_revision\":{policy_revision},",
                "\"policy_apps\":{policy_apps},",
                "\"events_count\":{events_count},",
                "\"events_total\":{events_total},",
                "\"frozen_packages\":{frozen},",
                "\"grace_pending\":{grace},",
"\"freeze_ops\":{freeze_ops},",
                "\"unfreeze_ops\":{unfreeze_ops},",
                "\"wakeup_thaws\":{wakeup_thaws},",
                "\"wake_throttled\":{wake_throttled},",
                "\"discard_ops\":{discard_ops},",
                "\"discard_reasons\":{{\"frozen_timeout\":{discard_ft},\"mem_watermark\":{discard_mw},\"boot_reclaim\":{discard_br}}},",
                "\"discarded_packages\":{discarded},",
                "\"discard_timeout_s\":{discard_timeout_s},",
                "\"uptime_s\":{uptime},",
                "\"config_reloads\":{reloads},",
                "\"connections_served\":{conns}",
                "}}"
            ),
            ver = paths::VERSION_NAME,
            rel = paths::RELEASE_NO,
            pid = std::process::id(),
            stub_loaded = stub_loaded,
            stub_hash = stub_hash_json,
            dex_version = dex_version_json,
            dex_match = dex_hash_match,
            bridge_hash = bridge_hash_json,
            bridge_match = bridge_hash_match,
            focus_pkg = focus_pkg_json,
            focus_changes = self.focus_changes.load(Ordering::Relaxed),
            wakeup_events = self.wakeup_events.load(Ordering::Relaxed),
            policy_enabled = policy_enabled,
            policy_revision = policy_revision,
            policy_apps = policy_apps,
            events_count = events_count,
            events_total = events_total,
            frozen = frozen_json,
            grace = grace_json,
            freeze_ops = freeze_ops,
            unfreeze_ops = unfreeze_ops,
            wakeup_thaws = wakeup_thaws,
            wake_throttled = wake_throttled,
            discard_ops = discard_ops,
            discard_ft = discard_frozen_timeout,
            discard_mw = discard_mem_watermark,
            discard_br = discard_boot_reclaim,
            discarded = discarded_json,
            discard_timeout_s = discard_timeout_s,
            uptime = self.uptime_secs(),
            reloads = self.config_reloads.load(Ordering::Relaxed),
            conns = self.connections_served.load(Ordering::Relaxed),
        )
    }
}

/// 字符串数组 → JSON 数组字面量
fn json_str_array(v: &[String]) -> String {
    let inner: Vec<String> = v.iter().map(|s| format!("\"{}\"", s)).collect();
    format!("[{}]", inner.join(","))
}
