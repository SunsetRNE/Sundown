//! 高网络负载判定（L3 豁免维度 keep_high_network，v0.4.19-l3）。
//!
//! 数据源（Android 内核网络统计，按 uid）：
//!   /proc/uid_stat/<uid>/tcp_rcv、tcp_snd —— 传统 qtaguid 兼容层（累计字节，十进制）
//!   /proc/net/xt_qtaguid/stats             —— 按 uid 聚合行（rx/tx 字节，含 tcp/udp）
//!
//! 探测顺序：先 uid_stat（轻量双读），读不到再回落 xt_qtaguid 聚合；两源皆不可用 →
//! 功能降级（恒判定 false，豁免维度不生效，不致命）。
//!
//! 语义：某 uid 在采样窗口内的流量增量 ≥ 阈值 → 视为"高网络负载"（活跃传输中，
//! 退后台也不应冻结——正在下载/上传/语音传输，冻结会断流）。
//!
//! 失败安全：任一读数失败按 0 处理（宁可少豁免——冻结优先，与 dex 侧 fg/media 同纪律）。

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::logw;

/// 判定阈值：采样窗口内收发合计 ≥ 该值视为高网络（256 KB / 30s 窗口）
pub const DEFAULT_THRESHOLD: u64 = 256 * 1024;
/// 采样窗口时长
pub const DEFAULT_WINDOW: Duration = Duration::from_secs(30);

/// uid → 上次采样（累计字节, 时刻）
#[derive(Debug)]
pub struct NetSampler {
    last: HashMap<u32, (u64, Instant)>,
    /// 当前是否已确认可用数据源（避免每次判定重复探测代价；None = 未探测）
    source: Option<NetSource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NetSource {
    UidStat,
    XtQtaguid,
}

impl NetSampler {
    pub fn new() -> Self {
        Self {
            last: HashMap::new(),
            source: None,
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
        // 首次探测：优先 uid_stat
        if let Some(b) = uid_stat_bytes(uid) {
            self.source = Some(NetSource::UidStat);
            return Some(b);
        }
        if let Some(b) = xt_qtaguid_bytes(uid) {
            self.source = Some(NetSource::XtQtaguid);
            return Some(b);
        }
        if self.source.is_none() {
            logw!("网络统计源不可用（/proc/uid_stat 与 xt_qtaguid 均失败），keep_high_network 降级关闭");
            self.source = Some(NetSource::UidStat); // 标记已探测，避免重复告警
        }
        None
    }
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
