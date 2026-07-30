//! Sundown 路径常量（与 NAMING.md / sunctl-spec.md 定稿一致）

pub const SUNDOWN_DIR: &str = "/data/adb/sundown";
pub const CONF_DIR: &str = "/data/adb/sundown/conf";
pub const DATA_DIR: &str = "/data/adb/sundown/data";
pub const LOG_DIR: &str = "/data/adb/sundown/logs";
pub const UPDATE_DIR: &str = "/data/adb/sundown/update";
pub const READY_MARKER: &str = "/data/adb/sundown/update/daemon.ready";
pub const INSTALLED_META: &str = "/data/adb/sundown/update/installed.json";
pub const SOCKET_PATH: &str = "/data/adb/sundown/sundownd.sock";
pub const LOG_FILE: &str = "/data/adb/sundown/logs/sundownd.log";

/// 守护进程版本（与 module.prop versionCode 同步策略见 README）
pub const VERSION_NAME: &str = "0.1.0-l0";
/// 单调递增的发布号：service.sh readiness 校验依据（installed.json vs daemon.ready）
pub const RELEASE_NO: u32 = 1;