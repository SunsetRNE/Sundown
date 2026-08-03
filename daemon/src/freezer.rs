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
use std::time::{Duration, Instant};

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

// ---------------- VPN 守护进程保护（v0.4.22-l3） ----------------
//
// 背景（实机反馈）：VPN 类 app（Clash 等）的 tun 隧道承载全局代理——被冻结 =
// 全网 app 断网（VPN 切后台冻死后其余 app 全部无网络）；且其主进程非 :push 类，
// push_mode=Keep 选择性冻结保不住，fg 软豁免依赖 dex 判定存在失效窗口。
// 方案：硬豁免——探测持有 tun 设备 fd 的进程所属 uid（VPN owner），该 uid 永不
// 冻结；另支持 policy [vpn] packages 手动兜底列表（引擎侧消费）。

/// tun 接口名判定（tun0/tun1/tun10...；排除 tunl0 等系统 IPIP 隧道）
fn is_tun_iface(name: &str) -> bool {
    name.len() > 3
        && name.starts_with("tun")
        && name.chars().skip(3).all(|c| c.is_ascii_digit())
}

fn has_tun_iface() -> bool {
    let Ok(text) = std::fs::read_to_string("/proc/net/dev") else {
        return false;
    };
    text.lines().any(|l| {
        let name = l.trim_start().split(':').next().unwrap_or("");
        is_tun_iface(name)
    })
}

/// 探测 VPN owner uid：持有 tun 设备 fd 的进程所属 uid（60s 缓存——全扫有成本）
static VPN_OWNER_CACHE: Mutex<Option<(Instant, Option<u32>)>> = Mutex::new(None);
const VPN_CACHE_TTL: Duration = Duration::from_secs(60);

pub fn vpn_owner_uid() -> Option<u32> {
    {
        let g = VPN_OWNER_CACHE.lock().unwrap();
        if let Some((t, v)) = g.as_ref() {
            if t.elapsed() < VPN_CACHE_TTL {
                return *v;
            }
        }
    }
    let v = scan_vpn_owner();
    *VPN_OWNER_CACHE.lock().unwrap() = Some((Instant::now(), v));
    v
}

/// 指定 uid 是否为当前 VPN owner（tun 隧道持有者）
pub fn is_vpn_owner(uid: u32) -> bool {
    vpn_owner_uid() == Some(uid)
}

fn scan_vpn_owner() -> Option<u32> {
    // 1) 无 tun 接口（VPN 未建立）→ None
    if !has_tun_iface() {
        return None;
    }
    // 2) 收集候选 pid：apps cgroup 下 uid_*/pid_*（比全 /proc 少且准确）；
    //    cgroup 不可读时回退全 /proc 遍历
    let mut pids: Vec<u32> = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/sys/fs/cgroup/apps") {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with("uid_") {
                continue;
            }
            if let Ok(rd2) = std::fs::read_dir(e.path()) {
                for e2 in rd2.flatten() {
                    let n2 = e2.file_name().to_string_lossy().to_string();
                    if let Some(rest) = n2.strip_prefix("pid_") {
                        if let Ok(pid) = rest.parse::<u32>() {
                            pids.push(pid);
                        }
                    }
                }
            }
        }
    }
    if pids.is_empty() {
        if let Ok(rd) = std::fs::read_dir("/proc") {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.chars().all(|c| c.is_ascii_digit()) {
                    if let Ok(pid) = name.parse::<u32>() {
                        pids.push(pid);
                    }
                }
            }
        }
    }
    // 3) 逐个进程扫 fd，命中 tun 设备 → 返回其 uid
    for pid in pids {
        let fd_dir = format!("/proc/{}/fd", pid);
        let Ok(rd) = std::fs::read_dir(&fd_dir) else {
            continue;
        };
        for e in rd.flatten() {
            let Ok(target) = std::fs::read_link(e.path()) else {
                continue;
            };
            let t = target.to_string_lossy();
            let hit = t.starts_with("/dev/tun") || t.contains("anon_inode:[tun");
            if hit {
                if let Some(uid) = pid_uid(pid) {
                    logi!("L3 VPN owner 探测命中: uid={} pid={} fd={}", uid, pid, t);
                    return Some(uid);
                }
            }
        }
    }
    None
}

/// uid 是否实际处于冻结（uid 层或任一 pid 子层 cgroup.freeze=1）。
/// 选择性冻结（keep_push）只写 pid 子层——必须两层都查。
pub fn uid_has_frozen_procs(uid: u32) -> bool {
    let base = format!("/sys/fs/cgroup/apps/uid_{}", uid);
    if let Ok(s) = std::fs::read_to_string(format!("{}/cgroup.freeze", base)) {
        if s.trim() == "1" {
            return true;
        }
    }
    let Ok(rd) = std::fs::read_dir(&base) else {
        return false;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with("pid_") {
            continue;
        }
        if let Ok(s) = std::fs::read_to_string(e.path().join("cgroup.freeze")) {
            if s.trim() == "1" {
                return true;
            }
        }
    }
    false
}

/// 全部冻结中 uid（uid 层或任一 pid 子层 =1）；对账/诊断用
pub fn frozen_uids() -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir("/sys/fs/cgroup/apps") else {
        return out;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with("uid_") {
            continue;
        }
        let Ok(uid) = name[4..].parse::<u32>() else {
            continue;
        };
        if let Ok(s) = std::fs::read_to_string(e.path().join("cgroup.freeze")) {
            if s.trim() == "1" {
                out.push(uid);
                continue;
            }
        }
        if let Ok(rd2) = std::fs::read_dir(e.path()) {
            for e2 in rd2.flatten() {
                let n2 = e2.file_name().to_string_lossy().to_string();
                if !n2.starts_with("pid_") {
                    continue;
                }
                if let Ok(s) = std::fs::read_to_string(e2.path().join("cgroup.freeze")) {
                    if s.trim() == "1" {
                        out.push(uid);
                        break;
                    }
                }
            }
        }
    }
    out
}

/// v0.4.29-l3：读上次会话冻结集持久化（行式 `pkg:uid`）——启动归属对账的权威源。
/// 文件由 engine 冻结表变化时落盘（tick 末尾 persist_frozen_if_changed）。
pub fn read_frozen_state() -> Vec<(String, u32)> {
    let mut out = Vec::new();
    let Ok(text) = std::fs::read_to_string(crate::paths::STATE_FROZEN_FILE) else {
        return out;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((pkg, uid_s)) = line.split_once(':') {
            if let Ok(uid) = uid_s.trim().parse::<u32>() {
                out.push((pkg.trim().to_string(), uid));
            }
        }
    }
    out
}

/// v0.4.29-l3：清空冻结集持久化（启动归属对账完成后；本会话从零开始）
pub fn clear_frozen_state() {
    let _ = std::fs::remove_file(crate::paths::STATE_FROZEN_FILE);
}

/// 启动/异常恢复：解冻全部残留冻结（uid 层 + pid 子层写 0）→ 返回解冻 uid 数。
/// daemon 崩溃/重启后 cgroup.freeze 状态保留（内核态），frozen 表却已清空——
/// 若不清理，残留冻结的 app 切前台也不会被解冻（表现为"能打开但点击无响应"）。
/// v0.4.29-l3 起**不再使用**（误解冻 HANS 冻结集——2026-08-03 实机教训）：
/// 归属对账见 main.rs 启动段（read_frozen_state + 持久化匹配才动作）。
/// 保留实现供回归参考（历史缺陷对照），故允许 dead_code。
#[allow(dead_code)]
pub fn thaw_all_residual() -> usize {
    let mut n = 0usize;
    let Ok(rd) = std::fs::read_dir("/sys/fs/cgroup/apps") else {
        return n;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.starts_with("uid_") {
            continue;
        }
        // uid 层
        let uid_path = e.path().join("cgroup.freeze");
        if let Ok(s) = std::fs::read_to_string(&uid_path) {
            if s.trim() == "1" && write_freeze(&uid_path.to_string_lossy(), "0") {
                n += 1;
            }
        }
        // pid 子层（选择性冻结残留）
        if let Ok(rd2) = std::fs::read_dir(e.path()) {
            for e2 in rd2.flatten() {
                let n2 = e2.file_name().to_string_lossy().to_string();
                if !n2.starts_with("pid_") {
                    continue;
                }
                let p = e2.path().join("cgroup.freeze");
                if let Ok(s) = std::fs::read_to_string(&p) {
                    if s.trim() == "1" {
                        let _ = write_freeze(&p.to_string_lossy(), "0");
                    }
                }
            }
        }
    }
    n
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

    #[test]
    fn tun_iface_match_v0422() {
        // v0.4.22-l3：VPN 隧道接口判定（tun0/tun1/tun10 命中；tunl0 等系统隧道排除）
        assert!(is_tun_iface("tun0"));
        assert!(is_tun_iface("tun1"));
        assert!(is_tun_iface("tun10"));
        assert!(is_tun_iface("tun123"));
        // 排除：系统 IPIP 隧道 / 无数字后缀 / 空
        assert!(!is_tun_iface("tunl0"));
        assert!(!is_tun_iface("tun"));
        assert!(!is_tun_iface(""));
        assert!(!is_tun_iface("wlan0"));
        assert!(!is_tun_iface("rmnet0"));
        // /proc/net/dev 行解析：接口名在冒号前（可带前导空格）
        let line = "  tun0: 1234 5678    0    0    0     0          0         0    1234 5678    0    0    0     0       0          0         0";
        let name = line.trim_start().split(':').next().unwrap_or("");
        assert!(is_tun_iface(name));
        let lo = "    lo: 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0";
        let n2 = lo.trim_start().split(':').next().unwrap_or("");
        assert!(!is_tun_iface(n2));
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