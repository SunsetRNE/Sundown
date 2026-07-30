//! 极简日志：写 /data/adb/sundown/logs/sundownd.log，带时间戳。
//! L0 阶段够用；后续可替换为环形缓冲/tracing。

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::paths;

static LOG_LOCK: Mutex<()> = Mutex::new(());

fn now_stamp() -> String {
    // 避免引入 chrono：用 epoch 秒 + 简易 UTC 分解（日志对时区不敏感）
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86400;
    let tod = secs % 86400;
    let (h, m, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    // 儒略日 -> 年月日（标准算法）
    let mut z = days as i64 + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    z = if mo <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02} {:02}:{:02}:{:02}Z", z, mo, d, h, m, s)
}

pub fn log(level: &str, msg: &str) {
    let line = format!("[{}] [{}] {}\n", now_stamp(), level, msg);
    let _guard = LOG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(paths::LOG_FILE) {
        let _ = f.write_all(line.as_bytes());
    }
    // 同时输出到 stdout：service.sh 已重定向到 boot_watchdog.log
    print!("{}", line);
}

#[macro_export]
macro_rules! logi {
    ($($arg:tt)*) => { $crate::logging::log("INFO", &format!($($arg)*)) };
}
#[macro_export]
macro_rules! logw {
    ($($arg:tt)*) => { $crate::logging::log("WARN", &format!($($arg)*)) };
}
#[macro_export]
macro_rules! loge {
    ($($arg:tt)*) => { $crate::logging::log("ERROR", &format!($($arg)*)) };
}