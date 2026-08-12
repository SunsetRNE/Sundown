//! C1（v0.8-l3）使用画像采集（per-app 聚合；零新增采集）
//!
//! 数据源 = 引擎事件入口旁路聚合（on_focus / on_wakeup / freeze_now / 解冻路径），
//! 不新增任何采集点——与"C 档行为学习最谨慎"定位一致（纯内存，无 IO 失败面）。
//!
//! 画像字段（per-app）：
//! - 前台：focus_count 次数 / focus_ms 累计时长（焦点进入→离开计时，切换即累计）
//! - 冻结：freeze_count / unfreeze_count / discard_count（终态）
//! - 唤醒：wakeup_count 总数 + wakeup_sources 源分布（broadcast/service/pendingintent）
//! - 时间轴：first_seen / last_seen / last_focus / last_freeze（epoch 秒）
//!
//! 铁律：
//! - 纯内存聚合（不落盘、不持久化——画像为诊断/建议输入，非审计数据；
//!   审计走 events.jsonl 既有通道）
//! - 失败安全：无 IO、无 panic 面；未知包名照常建画像（观测完整）
//! - 导出面：sock `profile` 命令（top/summary/get）+ sunctl profile 子命令

use std::collections::HashMap;
use std::time::Instant;

/// 单 app 使用画像
#[derive(Debug, Clone, Default)]
pub struct AppProfile {
    /// 前台次数（event focus 进入）
    pub focus_count: u64,
    /// 前台累计毫秒（焦点进入→离开）
    pub focus_ms: u64,
    /// 冻结次数（freeze_now 成功语义）
    pub freeze_count: u64,
    /// 解冻次数（决策驱动的解冻：前台/wakeup；残局清理不计）
    pub unfreeze_count: u64,
    /// 丢弃次数（v0.4.52-l3 超时丢弃终态：SIGKILL 释放内存）
    pub discard_count: u64,
    /// 唤醒事件总数（含被忽略/节流/门控的——"到达 daemon 的全部唤醒"）
    pub wakeup_count: u64,
    /// 唤醒源分布（source → 次数；诊断"哪种源在骚扰"）
    pub wakeup_sources: HashMap<String, u64>,
    /// 首次出现（epoch 秒）
    pub first_seen_at: u64,
    /// 最近出现（epoch 秒）
    pub last_seen_at: u64,
    /// 最近前台（epoch 秒）
    pub last_focus_at: u64,
    /// 最近冻结（epoch 秒）
    pub last_freeze_at: u64,
    /// 前台进入时刻（内部：焦点离开时累计 focus_ms）
    focus_since: Option<Instant>,
}

impl AppProfile {
    /// 焦点进入：计数 + 起表 + 时间轴
    fn on_focus(&mut self, now: Instant) {
        self.focus_count += 1;
        self.focus_since = Some(now);
        self.last_focus_at = epoch_secs();
        self.touch();
    }

    /// 焦点离开：累计前台时长（无进入记录则忽略——事件丢失容忍）
    fn on_leave(&mut self, now: Instant) {
        if let Some(since) = self.focus_since.take() {
            self.focus_ms += now.duration_since(since).as_millis() as u64;
        }
    }

    fn on_wakeup(&mut self, source: &str) {
        self.wakeup_count += 1;
        *self.wakeup_sources.entry(source.to_string()).or_insert(0) += 1;
        self.touch();
    }

    fn on_freeze(&mut self) {
        self.freeze_count += 1;
        self.last_freeze_at = epoch_secs();
        self.touch();
    }

    fn on_unfreeze(&mut self) {
        self.unfreeze_count += 1;
        self.touch();
    }

    fn on_discard(&mut self) {
        self.discard_count += 1;
        self.touch();
    }

    /// 时间轴维护（first/last seen）
    fn touch(&mut self) {
        let t = epoch_secs();
        if self.first_seen_at == 0 {
            self.first_seen_at = t;
        }
        self.last_seen_at = t;
    }
}

/// 画像表（挂 EngineState；引擎锁已持有时调用，无需额外锁）
#[derive(Debug, Default)]
pub struct ProfileTable {
    apps: HashMap<String, AppProfile>,
}

impl ProfileTable {
    /// 焦点进入（engine.on_focus 调用：先记新前台，再让调用方对旧前台调 on_leave）
    pub fn on_focus(&mut self, pkg: &str) {
        self.apps.entry(pkg.to_string()).or_default().on_focus(Instant::now());
    }

    /// 焦点离开（engine.on_focus 的 prev != pkg 分支调用）
    pub fn on_leave(&mut self, pkg: &str) {
        self.apps
            .entry(pkg.to_string())
            .or_default()
            .on_leave(Instant::now());
    }

    /// 唤醒（engine.on_wakeup 入口调用；B4 全局源统计的 per-app 视图）
    pub fn on_wakeup(&mut self, pkg: &str, source: &str) {
        self.apps.entry(pkg.to_string()).or_default().on_wakeup(source);
    }

    /// 冻结（engine.freeze_now 调用）
    pub fn on_freeze(&mut self, pkg: &str) {
        self.apps.entry(pkg.to_string()).or_default().on_freeze();
    }

    /// 解冻（决策驱动解冻成功路径；残局清理/启动对账不计——非"使用"语义）
    pub fn on_unfreeze(&mut self, pkg: &str) {
        self.apps.entry(pkg.to_string()).or_default().on_unfreeze();
    }

    /// 丢弃（engine.discard 终态）
    pub fn on_discard(&mut self, pkg: &str) {
        self.apps.entry(pkg.to_string()).or_default().on_discard();
    }

    /// app 数（summary 观测）
    pub fn app_count(&self) -> usize {
        self.apps.len()
    }

    /// 按唤醒次数降序 TOP n（"疯狂唤醒者"识别输入）；稳定排序（同唤醒按包名字典序）
    pub fn top_wakeups(&self, n: usize) -> Vec<(String, &AppProfile)> {
        let mut v: Vec<(String, &AppProfile)> = self
            .apps
            .iter()
            .map(|(k, p)| (k.clone(), p))
            .collect();
        v.sort_by(|a, b| {
            b.1.wakeup_count
                .cmp(&a.1.wakeup_count)
                .then(a.0.cmp(&b.0))
        });
        v.truncate(n);
        v
    }

    /// 总唤醒数（summary 观测；零新增采集下的骚扰总览）
    pub fn total_wakeups(&self) -> u64 {
        self.apps.values().map(|p| p.wakeup_count).sum()
    }

    /// 总冻结数（summary 观测）
    pub fn total_freezes(&self) -> u64 {
        self.apps.values().map(|p| p.freeze_count).sum()
    }

    /// 单 app 画像（get 导出）
    pub fn get(&self, pkg: &str) -> Option<&AppProfile> {
        self.apps.get(pkg)
    }
}

/// epoch 秒（失败回落 0——仅观测时间轴）
fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn focus_accumulates_duration() {
        let mut t = ProfileTable::default();
        t.on_focus("com.a");
        std::thread::sleep(Duration::from_millis(20));
        t.on_leave("com.a");
        let p = t.get("com.a").unwrap();
        assert_eq!(p.focus_count, 1);
        assert!(p.focus_ms >= 15); // 宽松下限（调度抖动容忍）
        assert!(p.last_focus_at > 0);
    }

    #[test]
    fn wakeup_counts_and_sources() {
        let mut t = ProfileTable::default();
        t.on_wakeup("com.a", "broadcast");
        t.on_wakeup("com.a", "broadcast");
        t.on_wakeup("com.a", "service");
        t.on_wakeup("com.b", "pendingintent");
        let p = t.get("com.a").unwrap();
        assert_eq!(p.wakeup_count, 3);
        assert_eq!(p.wakeup_sources.get("broadcast"), Some(&2));
        assert_eq!(p.wakeup_sources.get("service"), Some(&1));
        assert_eq!(t.total_wakeups(), 4);
        assert_eq!(t.app_count(), 2);
    }

    #[test]
    fn freeze_unfreeze_discard() {
        let mut t = ProfileTable::default();
        t.on_freeze("com.a");
        t.on_freeze("com.a");
        t.on_unfreeze("com.a");
        t.on_discard("com.a");
        let p = t.get("com.a").unwrap();
        assert_eq!(p.freeze_count, 2);
        assert_eq!(p.unfreeze_count, 1);
        assert_eq!(p.discard_count, 1);
        assert!(p.last_freeze_at > 0);
    }

    #[test]
    fn top_wakeups_order() {
        let mut t = ProfileTable::default();
        t.on_wakeup("com.a", "broadcast");
        t.on_wakeup("com.b", "service");
        t.on_wakeup("com.b", "service");
        let top = t.top_wakeups(1);
        assert_eq!(top.len(), 1);
        assert_eq!(top[0].0, "com.b");
        assert_eq!(top[0].1.wakeup_count, 2);
        // 同唤醒数按字典序
        let top2 = t.top_wakeups(2);
        assert_eq!(top2[0].0, "com.b");
        assert_eq!(top2[1].0, "com.a");
    }

    #[test]
    fn leave_without_focus_is_safe() {
        // 事件丢失容忍：无进入记录直接离开 → 不 panic、时长 0
        let mut t = ProfileTable::default();
        t.on_leave("com.x");
        let p = t.get("com.x").unwrap();
        assert_eq!(p.focus_ms, 0);
        assert_eq!(p.focus_count, 0);
    }
}