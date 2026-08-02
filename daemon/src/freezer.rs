//! L3 冻结执行：cgroup v2 freezer（docs/l3-plan.md §0.1）。
//!
//! 通道（真机取证 PJD110 / Android 16）：
//!   /sys/fs/cgroup/apps/uid_<uid>/cgroup.freeze   —— uid 级（整 app），写 1 冻结 / 0 解冻
//!   /sys/fs/cgroup/apps/uid_<uid>/pid_<pid>/cgroup.freeze —— pid 级（备选/诊断）
//!
//! uid 来源：/data/system/packages.list（pkg uid 行式，root 可读）——
//! pkg→uid 全量映射（含未运行包）。包表带 mtime 缓存，变更自动重读。

use std::collections::HashMap;
use std::sync::Mutex;

use crate::{loge, logi, logw, paths};

/// pkg→uid 缓存（进程内全局；包表小，mtime 变化才重读）
static PKG_UID_CACHE: Mutex<Option<(u64, HashMap<String, u32>)>> = Mutex::new(None);

/// 查询 pkg 的 uid（user 0）；查不到返回 None
pub fn pkg_uid(pkg: &str) -> Option<u32> {
    let mtime = file_mtime(paths::PACKAGES_LIST)?;
    {
        let guard = PKG_UID_CACHE.lock().unwrap();
        if let Some((mt, map)) = guard.as_ref() {
            if *mt == mtime {
                return map.get(pkg).copied();
            }
        }
    }
    // 重读
    let map = read_packages_list();
    let uid = map.get(pkg).copied();
    let mut guard = PKG_UID_CACHE.lock().unwrap();
    *guard = Some((mtime, map));
    uid
}

/// 全量重读 packages.list（pkg → uid）
fn read_packages_list() -> HashMap<String, u32> {
    let mut map = HashMap::new();
    let Ok(text) = std::fs::read_to_string(paths::PACKAGES_LIST) else {
        loge!("packages.list 读取失败: {}", paths::PACKAGES_LIST);
        return map;
    };
    for line in text.lines() {
        let mut it = line.split_whitespace();
        let (Some(pkg), Some(uid_str)) = (it.next(), it.next()) else {
            continue;
        };
        if let Ok(uid) = uid_str.parse::<u32>() {
            map.insert(pkg.to_string(), uid);
        }
    }
    if !map.is_empty() {
        logi!("packages.list 已加载: {} 个包", map.len());
    }
    map
}

fn file_mtime(path: &str) -> Option<u64> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// uid 级冻结路径
fn uid_freeze_path(uid: u32) -> String {
    format!("/sys/fs/cgroup/apps/uid_{}/cgroup.freeze", uid)
}

/// 冻结整个 app（uid 级）。成功 true；目录缺失/写失败 false（进程未运行或路径不存在）
pub fn freeze_uid(uid: u32) -> bool {
    write_freeze(&uid_freeze_path(uid), "1")
}

/// 解冻整个 app（uid 级）
pub fn unfreeze_uid(uid: u32) -> bool {
    write_freeze(&uid_freeze_path(uid), "0")
}

/// 查询 uid 冻结状态：Some(true)=冻结中，Some(false)=未冻结，None=路径不可读（未运行）
/// 诊断/验证 API（T4 冻结读回实证、status 冻结列表核验）；引擎内部当前未调用
#[allow(dead_code)]
pub fn is_uid_frozen(uid: u32) -> Option<bool> {
    let path = uid_freeze_path(uid);
    match std::fs::read_to_string(&path) {
        Ok(s) => Some(s.trim() == "1"),
        Err(_) => None,
    }
}

/// uid 目录下是否还有存活进程（2026-08-02 PJD110/Android16 实证：
/// app 进程挂在 apps/uid_X/pid_Y/ 子目录，uid_X/cgroup.procs **不递归包含**
/// 子目录进程（恒空）——必须遍历 pid_*；同时兜底兼容旧结构直接挂 uid 层。
/// 比 proc-add 事件可靠（dex 未上报 proc-add 前，pkg_pids 索引不可信）。
pub fn uid_has_procs(uid: u32) -> bool {
    let base = format!("/sys/fs/cgroup/apps/uid_{}", uid);
    let rd = match std::fs::read_dir(&base) {
        Ok(r) => r,
        Err(_) => return false, // 目录不存在 = 无进程
    };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("pid_") {
            let procs = entry.path().join("cgroup.procs");
            if let Ok(s) = std::fs::read_to_string(&procs) {
                if !s.trim().is_empty() {
                    return true;
                }
            }
        }
    }
    // 兜底：旧结构直接挂 uid 层
    if let Ok(s) = std::fs::read_to_string(format!("{}/cgroup.procs", base)) {
        if !s.trim().is_empty() {
            return true;
        }
    }
    false
}

/// 按包名冻结（经 packages.list 查 uid）；pkg 未知 → false
pub fn freeze_pkg(pkg: &str) -> bool {
    match pkg_uid(pkg) {
        Some(uid) => freeze_uid(uid),
        None => {
            logw!("冻结失败：包表未知 pkg={}", pkg);
            false
        }
    }
}

/// 按包名解冻；pkg 未知 → false（幂等：未冻结也返回 true 语义？——统一返回写结果）
pub fn unfreeze_pkg(pkg: &str) -> bool {
    match pkg_uid(pkg) {
        Some(uid) => unfreeze_uid(uid),
        None => {
            logw!("解冻失败：包表未知 pkg={}", pkg);
            false
        }
    }
}

// ---------------- 子进程管理（v0.4.19-l3，参考 Cerberus §6.3） ----------------
//
// cgroup v2 freezer 支持 pid 级子目录（apps/uid_X/pid_Y/cgroup.freeze），
// 选择性冻结 = 对除 :push 类外的每个 pid 子 cgroup 写 1（:push 保持运行，推送通道不断）。
// 解冻仍走 uid 层写 0（父层写 0 递归解冻整棵子树，含保留的 push——push 本就没冻，无副作用）。
//
// 风险与回落：Android 系统可能重排 cgroup 层级（AMS 进程管理）或 pid 子目录
// cgroup.freeze 不可写 → 选择性冻结失败 → 回落 uid 级整冻（宁多冻，保持冻结语义完整）。

/// :push 类子进程判定（cmdline 首参形如 "pkg:push" / "pkg:MSF" / "pkg:channel"）。
/// 匹配集合：push / msf / channel / pull（大小写不敏感）——微信 :push、QQ :MSF 等。
pub fn is_push_cmdline(first_arg: &str) -> bool {
    let Some(idx) = first_arg.rfind(':') else {
        return false;
    };
    let suffix = &first_arg[idx + 1..];
    let l = suffix.to_ascii_lowercase();
    l == "push" || l == "msf" || l == "channel" || l == "pull"
}

fn is_push_proc(pid: u32) -> bool {
    // cmdline 以 NUL 分隔；首个参数为进程名（可能带 :suffix）
    let Ok(text) = std::fs::read_to_string(format!("/proc/{}/cmdline", pid)) else {
        return false;
    };
    match text.split('\0').next() {
        Some(first) => is_push_cmdline(first),
        None => false,
    }
}

/// 选择性冻结：保留 :push 类子进程，冻结其余 pid 子 cgroup。
/// 无 pid 子目录 / 全部失败 → 回落 uid 级整冻。
pub fn freeze_pkg_keep_push(pkg: &str) -> bool {
    let Some(uid) = pkg_uid(pkg) else {
        logw!("冻结失败：包表未知 pkg={}", pkg);
        return false;
    };
    let base = format!("/sys/fs/cgroup/apps/uid_{}", uid);
    let rd = match std::fs::read_dir(&base) {
        Ok(r) => r,
        Err(_) => return freeze_uid(uid), // 目录不存在 = 无进程（写失败语义）
    };
    let mut found = 0usize;
    let mut frozen_any = false;
    let mut kept = 0usize;
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("pid_") {
            continue;
        }
        found += 1;
        let Ok(pid) = name[4..].parse::<u32>() else {
            continue;
        };
        if is_push_proc(pid) {
            kept += 1;
            logi!("L3 子进程保留（:push 类）: pid={} pkg={}", pid, pkg);
            continue;
        }
        let path = entry.path().join("cgroup.freeze");
        if write_freeze(&path.to_string_lossy(), "1") {
            frozen_any = true;
        }
    }
    if found == 0 {
        return freeze_uid(uid); // 无 pid 子目录（旧结构）→ 整冻回落
    }
    if !frozen_any && kept == 0 {
        return freeze_uid(uid); // 全部失败 → 整冻回落
    }
    if kept > 0 {
        logi!("L3 选择性冻结: {}（保留 {} 个 :push 子进程）", pkg, kept);
    }
    frozen_any
}

/// 冻结 + 连带杀死 :push 类子进程（kill 模式：通讯类 app 断推送彻底休眠）。
/// 杀进程用 SIGKILL（libc::kill；仅限该 uid 下的 :push 子进程，不波及其他）。
pub fn freeze_pkg_kill_push(pkg: &str) -> bool {
    let Some(uid) = pkg_uid(pkg) else {
        logw!("冻结失败：包表未知 pkg={}", pkg);
        return false;
    };
    kill_push_procs(uid);
    freeze_uid(uid)
}

fn kill_push_procs(uid: u32) {
    let base = format!("/sys/fs/cgroup/apps/uid_{}", uid);
    let Ok(rd) = std::fs::read_dir(&base) else {
        return;
    };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("pid_") {
            continue;
        }
        let Ok(pid) = name[4..].parse::<u32>() else {
            continue;
        };
        if is_push_proc(pid) {
            // 读 cgroup.procs 确认 pid 仍归属该 cgroup（防 pid 复用误杀）
            let procs = entry.path().join("cgroup.procs");
            let owned = std::fs::read_to_string(&procs)
                .map(|s| s.lines().any(|l| l.trim() == pid.to_string()))
                .unwrap_or(false);
            if !owned {
                continue;
            }
            let ret = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            logi!(
                "L3 子进程杀死（:push 类）: pid={} uid={} rc={}",
                pid,
                uid,
                ret
            );
        }
    }
}

/// 从 /proc/<pid>/status 解析 uid（proc-add 事件未带 uid 时的兜底）
pub fn pid_uid(pid: u32) -> Option<u32> {
    let text = std::fs::read_to_string(format!("/proc/{}/status", pid)).ok()?;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("Uid:") {
            // Uid: real effective saved fs —— 取 effective（第二列）
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() >= 2 {
                return parts[1].parse::<u32>().ok();
            }
            if let Some(first) = parts.first() {
                return first.parse::<u32>().ok();
            }
        }
    }
    None
}

fn write_freeze(path: &str, val: &str) -> bool {
    match std::fs::write(path, val.as_bytes()) {
        Ok(_) => true,
        Err(e) => {
            logw!("cgroup.freeze 写入失败 {}: {}（{}）", path, e, val);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uid_parse() {
        // Uid: 1000 1000 1000 1000
        assert_eq!(pid_parse_uid("Uid:\t1000\t1000\t1000\t1000\n"), Some(1000));
        assert_eq!(pid_parse_uid("Name:\tabc\n"), None);
    }

    #[test]
    fn push_cmdline_match() {
        // 命中：微信 :push / QQ :MSF / 通道 / pull
        assert!(is_push_cmdline("com.tencent.mm:push"));
        assert!(is_push_cmdline("com.tencent.mobileqq:MSF"));
        assert!(is_push_cmdline("com.x.y:channel"));
        assert!(is_push_cmdline("com.x.y:Pull"));
        // 未命中：主进程 / 其他子进程 / 无冒号
        assert!(!is_push_cmdline("com.tencent.mm"));
        assert!(!is_push_cmdline("com.tencent.mm:remote"));
        assert!(!is_push_cmdline("com.x.y:pushservice")); // 前缀不是独立 token
        assert!(!is_push_cmdline(""));
    }

    fn pid_parse_uid(text: &str) -> Option<u32> {
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("Uid:") {
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if parts.len() >= 2 {
                    return parts[1].parse::<u32>().ok();
                }
                if let Some(first) = parts.first() {
                    return first.parse::<u32>().ok();
                }
            }
        }
        None
    }
}