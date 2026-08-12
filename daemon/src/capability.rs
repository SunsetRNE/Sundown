//! B4（v0.8-l3）设备能力探测矩阵
//!
//! 对齐 network.rs `probe_source` 的"启动自检 + 缓存可用源"哲学：
//! daemon 启动时探测一次设备能力（cgroup freezer 层级 / process_madvise 支持 /
//! 网络数据源 / 唤醒源统计基线），经 sock 命令面导出（sunctl capability）。
//!
//! 铁律：
//! - 全部**只读**探测（不写 freezer、不冻结任何进程）；写权限验证走真实冻结路径
//! - **失败安全**：任何探测失败 = 能力缺失（None/不支持），不 panic、不阻塞主流程
//! - **零依赖 libc**（std::fs 只读 + 既有 syscall 封装复用）
//! - 探测结果仅观测面消费（日志/status/WebUI），不参与决策（决策仍走失败安全主链路）

/// cgroup freezer 可用层级（由高到低）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreezerLevel {
    /// 无可用 freezer（冻结走 SIGSTOP 兜底通道）
    None,
    /// cgroup v1 freezer 控制器（/sys/fs/cgroup/freezer/freezer.state）
    V1,
    /// cgroup v2 uid 级（/sys/fs/cgroup/apps/uid_<uid>/cgroup.freeze）——Android 16 主流
    UidV2,
    /// cgroup v2 仅 pid 级（apps/uid_<uid>/pid_<pid>/cgroup.freeze，uid 级缺失时）
    PidV2,
}

impl FreezerLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            FreezerLevel::None => "none",
            FreezerLevel::V1 => "v1_freezer",
            FreezerLevel::UidV2 => "uid_v2",
            FreezerLevel::PidV2 => "pid_v2",
        }
    }
}

/// 能力矩阵（启动探测一次；capability reprobe 可刷新）
#[derive(Debug, Clone)]
pub struct Capability {
    /// freezer 可用层级（freezer.rs 实际执行通道与 SIGSTOP 兜底决策依据）
    pub freezer: FreezerLevel,
    /// cgroup v2 标志（/sys/fs/cgroup/cgroup.controllers 存在）
    pub cgroup_v2: bool,
    /// 实测存在的 uid 级路径（探测用 uid=10000——第一个 app uid，稳定存在）
    pub freezer_uid_path: Option<String>,
    /// 实测存在的 pid 级路径（apps/uid_<uid>/pid_<pid>/cgroup.freeze）
    pub freezer_pid_path: Option<String>,
    /// v1 freezer 挂载点（freezer.state 存在时）
    pub freezer_v1_path: Option<String>,
    /// process_madvise(MADV_WILLNEED) 内核支持（对自身进程实测；解冻预热前置）
    pub madvise_willneed: bool,
    /// 网络流量数据源（engine.net.probe_source() 结果；keep_network 豁免数据可用性）
    pub net_source: String,
    /// 探测时间戳（unix 秒）
    pub probed_at: u64,
}

/// 探测用 uid（第一个 app uid；app 域 cgroup 稳定存在，探测零副作用）
const PROBE_UID: u32 = 10000;

/// 从磁盘探测设备能力（只读）。net_source 由调用方从 engine.net 探测传入
/// （避免 capability→engine 循环依赖；调用方已有 &mut EngineState 上下文）。
pub fn probe(net_source: String) -> Capability {
    // ---- cgroup v2 标志（Android 11+ 主流；Android 16 真机为 v2） ----
    let cgroup_v2 = std::path::Path::new("/sys/fs/cgroup/cgroup.controllers").exists();

    // ---- v2 uid 级 / pid 级路径探测 ----
    // 实机教训（v0.4.59-l3 首测）：uid_10000 不一定存在（ColorOS 从 uid_10066 起，
    // 10000-10065 无安装应用即无目录）。改为枚举 apps/ 下**第一个存在的 uid_* 目录**
    // （任何设备只要有 app 运行即存在），探测零副作用且覆盖真实冻结路径。
    let apps_base = "/sys/fs/cgroup/apps";
    let mut uid_path: Option<String> = None;
    let mut pid_path: Option<String> = None;
    if let Ok(rd) = std::fs::read_dir(apps_base) {
        let mut uid_dirs: Vec<String> = rd
            .flatten()
            .filter_map(|e| {
                let name = e.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("uid_") {
                    Some(e.path().to_string_lossy().to_string())
                } else {
                    None
                }
            })
            .collect();
        uid_dirs.sort(); // 稳定取最小 uid（首个 app uid）
        for dir in uid_dirs {
            let fp = format!("{}/cgroup.freeze", dir);
            if std::path::Path::new(&fp).exists() {
                uid_path = Some(fp);
                // pid 级：同 uid 目录下 pid_* 子目录的 cgroup.freeze
                if let Ok(sub) = std::fs::read_dir(&dir) {
                    for entry in sub.flatten() {
                        let name = entry.file_name();
                        let name = name.to_string_lossy();
                        if name.starts_with("pid_") {
                            let p = entry.path().join("cgroup.freeze");
                            if p.exists() {
                                pid_path = Some(p.to_string_lossy().to_string());
                                break;
                            }
                        }
                    }
                }
                break;
            }
        }
    }
    // 兜底：PROBE_UID 路径（无 app 运行的环境——cgroup 残留目录可能仍存在）
    if uid_path.is_none() {
        let fp = format!("/sys/fs/cgroup/apps/uid_{}/cgroup.freeze", PROBE_UID);
        if std::path::Path::new(&fp).exists() {
            uid_path = Some(fp);
        }
    }

    // ---- v1 freezer 控制器 ----
    let v1_state = "/sys/fs/cgroup/freezer/freezer.state";
    let v1_ok = std::path::Path::new(v1_state).exists();

    // ---- freezer 层级判定（纯函数 classify，可单测） ----
    let freezer = classify(cgroup_v2, uid_path.is_some(), pid_path.is_some(), v1_ok);

    // ---- process_madvise(MADV_WILLNEED) 支持（对自身进程实测；失败 = 不支持） ----
    // 复用 freezer.rs 既有 syscall 封装（pidfd_open 434 / process_madvise 440）；
    // 对自身预热一次无副作用（启动期自检成本一次，与 network probe_source 同哲学）。
    let self_pid = std::process::id();
    let madvise_willneed = crate::freezer::probe_madvise_willneed(self_pid);

    Capability {
        freezer,
        cgroup_v2,
        freezer_uid_path: uid_path,
        freezer_pid_path: pid_path,
        freezer_v1_path: if v1_ok { Some(v1_state.to_string()) } else { None },
        madvise_willneed,
        net_source,
        probed_at: unix_now(),
    }
}

/// freezer 层级判定（纯函数：给定探测事实 → 级别；可单测）
/// 优先级：v2 uid 级 > v2 pid 级 > v1 > none（v2 标志仅用于观测，不参与判定）
fn classify(cgroup_v2: bool, uid_ok: bool, pid_ok: bool, v1_ok: bool) -> FreezerLevel {
    let _ = cgroup_v2; // 观测面字段；判定以实际路径为准（防御性：v2 标志存在但路径异常仍降级）
    if uid_ok {
        FreezerLevel::UidV2
    } else if pid_ok {
        FreezerLevel::PidV2
    } else if v1_ok {
        FreezerLevel::V1
    } else {
        FreezerLevel::None
    }
}

/// unix 秒（失败回落 0——仅观测面时间戳，不影响决策）
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_priority_uid_over_pid_over_v1() {
        // v2 uid 级存在 → UidV2（即使 pid 级/v1 也存在）
        assert_eq!(classify(true, true, true, true), FreezerLevel::UidV2);
        assert_eq!(classify(true, true, false, true), FreezerLevel::UidV2);
        // uid 缺失但 pid 级存在 → PidV2
        assert_eq!(classify(true, false, true, false), FreezerLevel::PidV2);
        assert_eq!(classify(true, false, true, true), FreezerLevel::PidV2);
        // 仅 v1 → V1
        assert_eq!(classify(false, false, false, true), FreezerLevel::V1);
        assert_eq!(classify(true, false, false, true), FreezerLevel::V1);
        // 全无 → None（SIGSTOP 兜底）
        assert_eq!(classify(false, false, false, false), FreezerLevel::None);
        assert_eq!(classify(true, false, false, false), FreezerLevel::None);
    }

    #[test]
    fn freezer_level_str() {
        assert_eq!(FreezerLevel::None.as_str(), "none");
        assert_eq!(FreezerLevel::V1.as_str(), "v1_freezer");
        assert_eq!(FreezerLevel::UidV2.as_str(), "uid_v2");
        assert_eq!(FreezerLevel::PidV2.as_str(), "pid_v2");
    }

    #[test]
    fn probe_never_panics_on_missing_paths() {
        // 探测函数在真实磁盘上运行；真机必有 cgroup（Android 16 v2），
        // 但本测试只验证"不 panic + 字段可读"（失败安全契约）。
        let cap = probe("unknown".to_string());
        let _ = cap.freezer.as_str();
        let _ = cap.cgroup_v2;
        let _ = cap.freezer_uid_path;
        let _ = cap.freezer_pid_path;
        let _ = cap.freezer_v1_path;
        let _ = cap.madvise_willneed;
        let _ = cap.net_source;
        let _ = cap.probed_at;
    }
}
