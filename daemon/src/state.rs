//! 共享状态与 status JSON 构造。
//! 输出字段向后兼容 docs/sunctl-spec.md 的 status --json 契约（只增不改）。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use crate::paths;

pub struct DaemonState {
    pub started_at: Instant,
    pub config_reloads: AtomicU64,
    pub connections_served: AtomicU64,
}

impl DaemonState {
    pub fn new() -> Self {
        Self {
            started_at: Instant::now(),
            config_reloads: AtomicU64::new(0),
            connections_served: AtomicU64::new(0),
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

    /// 兼容 sunctl-spec 的 status JSON（socket 应答版）。
    /// probe_* 字段 L0 恒为占位，L1/L2 阶段填真实值。
    pub fn status_json(&self) -> String {
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
                "\"probe_stub_loaded\":0,",
                "\"probe_dex_version\":null,",
                "\"uptime_s\":{uptime},",
                "\"config_reloads\":{reloads},",
                "\"connections_served\":{conns}",
                "}}"
            ),
            ver = paths::VERSION_NAME,
            rel = paths::RELEASE_NO,
            pid = std::process::id(),
            uptime = self.uptime_secs(),
            reloads = self.config_reloads.load(Ordering::Relaxed),
            conns = self.connections_served.load(Ordering::Relaxed),
        )
    }
}
