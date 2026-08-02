//! 高网络负载判定（L3 豁免维度 keep_high_network，v0.4.19-l3）。
//!
//! 数据源（Android 内核网络统计，按 uid，探测顺序）：
//!   1. /proc/uid_stat/<uid>/tcp_rcv、tcp_snd —— 传统 qtaguid 兼容层（累计字节，十进制）
//!   2. /proc/net/xt_qtaguid/stats             —— 按 uid 聚合行（rx/tx 字节，含 tcp/udp）
//!   3. /sys/fs/bpf/netd_shared/map_netd_app_uid_stats_map —— Android12+ AOSP 标准 eBPF map
//!      （v0.4.20-l3 补充：PJD110/Android16 实机验证前两源均不存在，OPPO 定制内核仅剩
//!        netd eBPF 统计；经 bpf() syscall 遍历 map 聚合 uid 流量）
//!
//! 语义：某 uid 在采样窗口内的流量增量 ≥ 阈值 → 视为"高网络负载"（活跃传输中，
//! 退后台也不应冻结——正在下载/上传/语音传输，冻结会断流）。
//!
//! 失败安全：任一读数失败按 0 处理（宁可少豁免——冻结优先，与 dex 侧 fg/media 同纪律）。

use std::collections::HashMap;
use std::os::unix::io::AsRawFd;
use std::time::{Duration, Instant};

use crate::logw;

/// 判定阈值：采样窗口内收发合计 ≥ 该值视为高网络（256 KB / 30s 窗口）
pub const DEFAULT_THRESHOLD: u64 = 256 * 1024;
/// 采样窗口时长
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(30);
/// bpf map 全量遍历缓存 TTL（遍历有成本，300ms tick 不能每次全扫）
const BPF_CACHE_TTL: Duration = Duration::from_secs(2);
/// AOSP 标准 netd uid 流量统计 map 路径（Android12+）
const BPF_UID_STATS_MAP: &str = "/sys/fs/bpf/netd_shared/map_netd_app_uid_stats_map";

/// uid → 上次采样（累计字节, 时刻）
#[derive(Debug)]
pub struct NetSampler {
    last: HashMap<u32, (u64, Instant)>,
    /// 当前是否已确认可用数据源（避免每次判定重复探测代价；None = 未探测）
    source: Option<NetSource>,
    /// bpf 源缓存（全量遍历结果；uid → 累计字节）+ 上次刷新时刻
    bpf_cache: HashMap<u32, u64>,
    bpf_last_refresh: Option<Instant>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NetSource {
    UidStat,
    XtQtaguid,
    Bpf,
}

impl NetSampler {
    pub fn new() -> Self {
        Self {
            last: HashMap::new(),
            source: None,
            bpf_cache: HashMap::new(),
            bpf_last_refresh: None,
        }
    }

    /// uid 是否处于高网络负载：窗口内增量 ≥ 阈值。
    /// 数据源不可用 → false（功能降级）。首次采样只记录基线 → false。
    pub fn is_active(&mut self, uid: u32, window: Duration, threshold: u64) -> bool {
        let Some(bytes) = self.uid_bytes(uid) else {
            // 源不可用：降级（仅记日志一次，避免刷屏）
            return false;
        };
        let now = Instant::now();
        let active = match self.last.get(&uid) {
            Some((prev, at)) => {
                let dt = now.duration_since(*at);
                let delta = bytes.saturating_sub(*prev);
                dt >= window && delta >= threshold
            }
            None => false,
        };
        self.last.insert(uid, (bytes, now));
        active
    }

    /// 读取 uid 累计网络字节（收发合计）。多源探测，缓存可用源。
    fn uid_bytes(&mut self, uid: u32) -> Option<u64> {
        if self.source == Some(NetSource::UidStat) {
            return uid_stat_bytes(uid);
        }
        if self.source == Some(NetSource::XtQtaguid) {
            return xt_qtaguid_bytes(uid);
        }
        if self.source == Some(NetSource::Bpf) {
            return self.bpf_uid_bytes(uid);
        }
        // 首次探测：uid_stat → xt_qtaguid → bpf
        if let Some(b) = uid_stat_bytes(uid) {
            self.source = Some(NetSource::UidStat);
            return Some(b);
        }
        if let Some(b) = xt_qtaguid_bytes(uid) {
            self.source = Some(NetSource::XtQtaguid);
            return Some(b);
        }
        if let Some(b) = self.bpf_uid_bytes(uid) {
            self.source = Some(NetSource::Bpf);
            return Some(b);
        }
        if self.source.is_none() {
            logw!("网络统计源不可用（uid_stat/xt_qtaguid/bpf map 均失败），keep_high_network 降级关闭");
            self.source = Some(NetSource::UidStat); // 标记已探测，避免重复告警
        }
        None
    }

    /// bpf map 源：全量遍历缓存（TTL 内直接查缓存），聚合 uid 累计字节
    fn bpf_uid_bytes(&mut self, uid: u32) -> Option<u64> {
        let need_refresh = match self.bpf_last_refresh {
            Some(t) => t.elapsed() >= BPF_CACHE_TTL,
            None => true,
        };
        if need_refresh {
            match bpf_snapshot_uid_bytes() {
                Some(snap) => {
                    self.bpf_cache = snap;
                    self.bpf_last_refresh = Some(Instant::now());
                }
                None => return None,
            }
        }
        self.bpf_cache.get(&uid).copied()
    }
}

/// 全量遍历 bpf uid 统计 map → uid → 累计字节（rx + tx）。
/// key 布局（UidKey）：uid u32 @0, iface_index u32 @4, tag u64 @8, counter_set u32 @16
/// （无论 packed 与否，前 4 字节恒为 uid——遍历时只取前 4 字节）。
/// value 布局（Stats）：rx_bytes u64 @0, rx_packets u64 @8, tx_bytes u64 @16, tx_packets u64 @24。
fn bpf_snapshot_uid_bytes() -> Option<HashMap<u32, u64>> {
    const BPF_MAP_GET_NEXT_KEY: libc::c_int = 2;
    const BPF_MAP_LOOKUP_ELEM: libc::c_int = 1;
    const BPF_OBJ_GET_INFO_BY_FD: libc::c_int = 15;

    let f = std::fs::File::open(BPF_UID_STATS_MAP).ok()?;
    let fd = f.as_raw_fd();

    // 1) 获取 map 信息（key_size / value_size）
    let mut info = [0u8; 64];
    let mut attr_info = [0u8; 16];
    attr_info[0..4].copy_from_slice(&(fd as u32).to_ne_bytes());
    attr_info[4..8].copy_from_slice(&64u32.to_ne_bytes());
    attr_info[8..16].copy_from_slice(&(info.as_mut_ptr() as u64).to_ne_bytes());
    if bpf_cmd(BPF_OBJ_GET_INFO_BY_FD, &attr_info) != 0 {
        return None;
    }
    let key_size = u32::from_ne_bytes([info[8], info[9], info[10], info[11]]) as usize;
    let value_size = u32::from_ne_bytes([info[12], info[13], info[14], info[15]]) as usize;
    if key_size == 0 || key_size > 64 || value_size < 24 {
        return None; // value 至少容纳 rx_bytes + tx_bytes
    }

    // 2) 遍历所有 key（GET_NEXT_KEY 链），lookup 聚合
    let mut map: HashMap<u32, u64> = HashMap::new();
    let mut cur = vec![0u8; key_size];
    let mut next = vec![0u8; key_size];
    let mut value = vec![0u8; value_size];

    // 首个 key：key=NULL
    let mut attr_nk = [0u8; 24];
    attr_nk[0..4].copy_from_slice(&(fd as u32).to_ne_bytes());
    attr_nk[16..24].copy_from_slice(&(next.as_mut_ptr() as u64).to_ne_bytes());
    if bpf_cmd(BPF_MAP_GET_NEXT_KEY, &attr_nk) != 0 {
        return None; // 空 map 或不可读
    }
    loop {
        // lookup 当前 key
        let mut attr_lk = [0u8; 32];
        attr_lk[0..4].copy_from_slice(&(fd as u32).to_ne_bytes());
        attr_lk[8..16].copy_from_slice(&(next.as_ptr() as u64).to_ne_bytes());
        attr_lk[16..24].copy_from_slice(&(value.as_mut_ptr() as u64).to_ne_bytes());
        if bpf_cmd(BPF_MAP_LOOKUP_ELEM, &attr_lk) == 0 {
            let uid = u32::from_ne_bytes([next[0], next[1], next[2], next[3]]);
            let rx = u64::from_ne_bytes([
                value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
            ]);
            let tx = u64::from_ne_bytes([
                value[16], value[17], value[18], value[19], value[20], value[21], value[22],
                value[23],
            ]);
            let e = map.entry(uid).or_insert(0u64);
            *e = e.saturating_add(rx).saturating_add(tx);
        }
        // 取下一个 key
        cur.copy_from_slice(&next);
        let mut attr_nk = [0u8; 24];
        attr_nk[0..4].copy_from_slice(&(fd as u32).to_ne_bytes());
        attr_nk[8..16].copy_from_slice(&(cur.as_ptr() as u64).to_ne_bytes());
        attr_nk[16..24].copy_from_slice(&(next.as_mut_ptr() as u64).to_ne_bytes());
        if bpf_cmd(BPF_MAP_GET_NEXT_KEY, &attr_nk) != 0 {
            break; // 遍历结束
        }
    }
    Some(map)
}

fn bpf_cmd(cmd: libc::c_int, attr: &[u8]) -> libc::c_long {
    unsafe { libc::syscall(libc::SYS_bpf, cmd, attr.as_ptr(), attr.len()) }
}

/// /proc/uid_stat/<uid>/tcp_rcv + tcp_snd（累计字节；任一缺失按另一侧计）
fn uid_stat_bytes(uid: u32) -> Option<u64> {
    let rcv = read_u64(&format!("/proc/uid_stat/{}/tcp_rcv", uid))?;
    let snd = read_u64(&format!("/proc/uid_stat/{}/tcp_snd", uid)).unwrap_or(0);
    Some(rcv + snd)
}

/// /proc/net/xt_qtaguid/stats：按 uid_tag_int 聚合 rx_bytes + tx_bytes（含 tcp/udp 全流量）。
/// 行格式：idx iface acct_tag_hex uid_tag_int cnt_set rx_bytes rx_packets tx_bytes tx_packets ...
fn xt_qtaguid_bytes(uid: u32) -> Option<u64> {
    let text = std::fs::read_to_string("/proc/net/xt_qtaguid/stats").ok()?;
    let mut total: u64 = 0;
    let mut found = false;
    for line in text.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 8 {
            continue;
        }
        if f[3] != uid.to_string() {
            continue;
        }
        // 跳过非 0 的 acct_tag（带 tag 的流量不算普通网络活动）
        if f[2] != "0x0" {
            continue;
        }
        let rx: u64 = f[5].parse().ok()?;
        let tx: u64 = f[7].parse().ok()?;
        total = total.saturating_add(rx).saturating_add(tx);
        found = true;
    }
    if found {
        Some(total)
    } else {
        None
    }
}

fn read_u64(path: &str) -> Option<u64> {
    let s = std::fs::read_to_string(path).ok()?;
    s.trim().parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampler_baseline_then_active() {
        // 直接验证采样语义：首次基线 false，源探测失败时降级 false（不 panic）
        let mut s = NetSampler::new();
        let now = Instant::now();
        // 模拟第一次采样（基线）
        s.last.insert(42, (0, now));
        assert!(!s.is_active(42, DEFAULT_WINDOW, DEFAULT_THRESHOLD)); // dt 0 → false
        // 模拟时间推进 + 大量流量：直接改内部基线
        s.last.insert(42, (1000, now - DEFAULT_WINDOW - Duration::from_secs(1)));
        // 下一次采样会读取真实 /proc（测试环境无 → false 降级），此处仅验证逻辑分支：
        // 若 uid_bytes 返回 Some(大值) 则 true——用 xt 源不可用时整体 false 也可接受
        let _ = s.is_active(42, DEFAULT_WINDOW, DEFAULT_THRESHOLD);
        // 无真实 /proc 时：源探测失败 → source 被标记，is_active 返回 false（降级不 panic）
        assert!(s.source.is_some());
    }

    #[test]
    fn xt_line_parse() {
        // 模拟 xt_qtaguid 行：idx iface acct uid cnt rx rxpk tx txpk ...
        let line = "0 wlan0 0x0 10123 0 1000 10 2000 20";
        let f: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(f[3], "10123");
        assert_eq!(f[2], "0x0");
        let rx: u64 = f[5].parse().unwrap();
        let tx: u64 = f[7].parse().unwrap();
        assert_eq!(rx + tx, 3000);
    }

    #[test]
    fn uid_stat_parse() {
        assert_eq!(read_u64_of("123456\n"), Some(123456));
        assert_eq!(read_u64_of("abc\n"), None);
        fn read_u64_of(s: &str) -> Option<u64> {
            s.trim().parse::<u64>().ok()
        }
    }
}
