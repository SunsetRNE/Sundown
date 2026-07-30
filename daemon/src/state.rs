//! 共享状态与 status JSON 构造。
//! 输出字段向后兼容 docs/sunctl-spec.md 的 status --json 契约（只增不改）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use crate::paths;

/// L1 探针桩的 hello-probe 上报记录
pub struct ProbeReport {
    pub build_hash: String,
    /// 距 daemon 启动的秒数（单调时钟，免系统时间跳变）
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
}

/// 从模块目录读取期望 hash（启动 / reload-config 时调用）
fn read_expected_hash() -> Option<String> {
    std::fs::read_to_string(paths::PROBE_EXPECTED_HASH_FILE)
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
        }
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
    }

    /// 兼容 sunctl-spec 的 status JSON（socket 应答版）。
    /// L1 起 probe_stub_loaded / probe_stub_build_hash 填真实值。
    pub fn status_json(&self) -> String {
        let (stub_loaded, stub_hash_json) = {
            let guard = self.probe.lock().unwrap();
            match guard.as_ref() {
                Some(p) => (1, format!("\"{}\"", p.build_hash)),
                None => (0, "null".to_string()),
            }
        };
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
                "\"probe_dex_version\":null,",
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
            uptime = self.uptime_secs(),
            reloads = self.config_reloads.load(Ordering::Relaxed),
            conns = self.connections_served.load(Ordering::Relaxed),
        )
    }
}
