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

// ---- L1 探针桩相关 ----
/// probe.dex / oat 目录（L2 资产落点）
pub const PROBE_DIR: &str = "/data/adb/sundown/probe";
pub const PROBE_DEX: &str = "/data/adb/sundown/probe/probe.dex";
pub const PROBE_OAT_DIR: &str = "/data/adb/sundown/probe/oat";
/// 期望的桩 build hash（CI 打包时写入模块，daemon 据此比对 hello-probe 上报值）
pub const PROBE_EXPECTED_HASH_FILE: &str = "/data/adb/modules/sundown/zygisk/probe.hash";

/// 守护进程版本（与 module.prop version 同步，策略见主 README「版本号策略」）
pub const VERSION_NAME: &str = "0.2.0-l1";
/// 单调递增的发布号：service.sh readiness 校验依据（installed.json vs daemon.ready）
/// daemon 二进制任何变更必须 +1（只加不改）
pub const RELEASE_NO: u32 = 2;