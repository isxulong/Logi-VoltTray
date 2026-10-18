//! WP-A（A-1b）：unix 秒 → 本地时「MM-DD HH:MM」注释（菜单虚拟条目用）。
//! Win32 FileTime 换算；任一步失败降级为「时间未知」，不 panic。
#![cfg(windows)]

use windows::Win32::Foundation::{FILETIME, SYSTEMTIME};
use windows::Win32::Storage::FileSystem::FileTimeToLocalFileTime;
use windows::Win32::System::Time::FileTimeToSystemTime;

/// 把 unix 秒格式化为本地「MM-DD HH:MM」。
pub fn fmt_hhmm(unix_secs: u64) -> String {
    // FILETIME：自 1601-01-01 UTC 起的 100ns 计数。
    const TICKS_PER_SEC: u64 = 10_000_000;
    const EPOCH_DIFF_SECS: u64 = 11_644_473_600; // 1601 → 1970
    let ticks = unix_secs
        .saturating_add(EPOCH_DIFF_SECS)
        .saturating_mul(TICKS_PER_SEC);
    let ft = FILETIME {
        dwLowDateTime: (ticks & 0xFFFF_FFFF) as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };
    unsafe {
        let mut lft = FILETIME::default();
        if FileTimeToLocalFileTime(&ft, &mut lft).is_err() {
            return "时间未知".to_string();
        }
        let mut st = SYSTEMTIME::default();
        if FileTimeToSystemTime(&lft, &mut st).is_err() {
            return "时间未知".to_string();
        }
        format!(
            "{:02}-{:02} {:02}:{:02}",
            st.wMonth, st.wDay, st.wHour, st.wMinute
        )
    }
}
