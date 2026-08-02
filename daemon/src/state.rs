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
    /// 元素为 (订阅 id, 可写副本)，id 单调分配，断连/写失败剔除
    pub dex_clients: Mutex<Vec<(u64, UnixStream)>>,
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

    /// 初始策略：读 conf/policy.toml，失败用默认（策略关闭，观测优先）
    fn init_engine() -> EngineState {
        let mut e = EngineState::default();
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
        self.dex_clients.lock().unwrap().push((id, stream));
        id
    }

    pub fn unregister_dex_client(&self, id: u64) {
        self.dex_clients.lock().unwrap().retain(|(i, _)| *i != id);
    }

    /// push-dex 广播：向所有订阅连接写事件头行 + dex 字节帧；
    /// 写失败的连接视为断连剔除。返回成功通知的订阅者数量。
    pub fn broadcast_dex(&self, header_line: &[u8], payload: &[u8]) -> usize {
        let mut notified = 0usize;
        self.dex_clients.lock().unwrap().retain_mut(|(_, s)| {
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
        ) = (
            eng.policy.enabled,
            eng.policy.revision,
            eng.policy.apps.len(),
            eng.events.len(),
            eng.events.total,
            eng.freeze_ops,
            eng.unfreeze_ops,
            eng.wakeup_thaws,
        );
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
