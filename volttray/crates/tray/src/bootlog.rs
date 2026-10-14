//! 启动可见化（v0.3.7，D-63）：托盘应用无主窗口，release 版是 windows
//! 子系统（无控制台），任何启动阶段的静默失败在用户侧就是「双击没反应」。
//! 本模块把启动过程全程落到日志文件，并在关键失败点弹中文 MessageBox：
//!
//! - **崩溃日志**：优先 `%APPDATA%\LogiVoltTray\logi-volttray.log`，EXE 同
//!   目录、`%TEMP%` 逐级兜底；全部失败则日志禁用（弹窗文案仍给出首选路径）。
//!   超过 1MB 的旧日志在打开前轮转，避免常年累月无限增长。
//! - **里程碑**：single-instance / dpi / aumid-shortcut / config-load /
//!   io-thread / icon-cache / tray-build / rawinput / startup-toast /
//!   message-pump / settings-open 各阶段各写一条；失败阶段写明原因。
//! - **panic hook**：所有 panic 落日志；仅主（GUI）线程额外弹窗——IO 线程
//!   panic 弹模态框会卡死后台线程，只留痕不打断。
//! - **单实例**：第二实例退出前弹「已在运行」提示（B-3）。
//!
//! 日志任何失败都静默吞掉：诊断手段自身绝不允许阻断启动。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use windows::core::PCWSTR;
use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
};

/// 日志文件名（固定，位于日志目录内）。
const LOG_FILE: &str = "logi-volttray.log";
/// 旧日志超过该大小即轮转（截断重开）。
const ROTATE_BYTES: u64 = 1_000_000;

static LOG: OnceLock<Mutex<Option<File>>> = OnceLock::new();
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
static MAIN_TID: OnceLock<u32> = OnceLock::new();

/// 本地时间 → `YYYY-MM-DD HH:MM:SS.mmm`（纯格式化，可单测）。
fn format_local_time(st: &SYSTEMTIME) -> String {
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond, st.wMilliseconds
    )
}

fn now_str() -> String {
    let st = unsafe { GetLocalTime() };
    format_local_time(&st)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 在指定目录打开（必要时创建）日志文件；超限旧日志先轮转。
fn open_in(dir: &Path) -> Option<File> {
    std::fs::create_dir_all(dir).ok()?;
    let path = dir.join(LOG_FILE);
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > ROTATE_BYTES {
            let _ = std::fs::remove_file(&path);
        }
    }
    OpenOptions::new().create(true).append(true).open(path).ok()
}

/// 初始化启动日志（幂等）：逐级尝试 APPDATA 配置目录 → EXE 同目录 → TEMP。
/// 所有候选都失败时日志禁用，`path_str` 仍返回首选路径供弹窗展示。
pub fn init() {
    if LOG.get().is_some() {
        return;
    }
    let appdata = std::env::var_os("APPDATA").map(|v| PathBuf::from(v).join("LogiVoltTray"));
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    let temp = std::env::var_os("TEMP").map(PathBuf::from);
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = appdata.as_ref() {
        dirs.push(d.clone());
    }
    if let Some(d) = exe_dir.as_ref() {
        dirs.push(d.clone());
    }
    if let Some(d) = temp.as_ref() {
        dirs.push(d.clone());
    }
    let preferred = dirs
        .first()
        .map(|d| d.join(LOG_FILE))
        .unwrap_or_else(|| PathBuf::from(LOG_FILE));
    for d in &dirs {
        if let Some(f) = open_in(d) {
            let _ = LOG_PATH.set(d.join(LOG_FILE));
            let _ = LOG.set(Mutex::new(Some(f)));
            note(&format!(
                "=== LogiVoltTray 启动 pid={} ver={} ===",
                std::process::id(),
                env!("CARGO_PKG_VERSION")
            ));
            return;
        }
    }
    // 日志全部失败：仍登记首选路径（弹窗文案用），写日志退化为空操作。
    let _ = LOG.set(Mutex::new(None));
    let _ = LOG_PATH.set(preferred);
}

/// 启动阶段里程碑。
pub fn stage(name: &str) {
    write_line(&format!("stage: {name}"));
}

/// 普通记录（失败原因、降级信息等）。
pub fn note(msg: &str) {
    write_line(&format!("note: {msg}"));
}

fn write_line(line: &str) {
    if let Some(m) = LOG.get() {
        if let Ok(mut guard) = m.lock() {
            if let Some(f) = guard.as_mut() {
                let _ = writeln!(f, "[{}] {}", now_str(), line);
            }
        }
    }
}

/// 日志文件路径（文案用；未初始化时退回固定文件名）。
pub fn path_str() -> String {
    LOG_PATH
        .get()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| LOG_FILE.to_string())
}

/// 中文致命错误弹窗（含日志路径），用于「即将退出」的启动失败点。
pub fn fatal_box(title: &str, body: &str) {
    let text = wide(&format!("{body}\n\n详细日志：{}", path_str()));
    let caption = wide(title);
    unsafe {
        let _ = MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(caption.as_ptr()),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND | MB_TOPMOST,
        );
    }
}

/// 第二实例提示（B-3）：单实例互斥已持有时，退出前让用户明确感知。
pub fn already_running_box() {
    let text = wide("LogiVoltTray 已在运行。\n\n本程序是托盘应用，没有主窗口：请查看任务栏右下角通知区域（可能收进了溢出箭头「^」里），右键图标可打开设置或退出。");
    let caption = wide("LogiVoltTray 已在运行");
    unsafe {
        let _ = MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(caption.as_ptr()),
            MB_OK | MB_ICONINFORMATION | MB_SETFOREGROUND | MB_TOPMOST,
        );
    }
}

/// panic hook（B-5）：所有 panic 落日志；仅主（GUI）线程弹窗后由
/// main 的 catch_unwind 收尾退出。
pub fn install_panic_hook() {
    let main_tid = unsafe { GetCurrentThreadId() };
    let _ = MAIN_TID.set(main_tid);
    std::panic::set_hook(Box::new(move |info| {
        write_line(&format!("PANIC: {info}"));
        let cur = unsafe { GetCurrentThreadId() };
        let on_main = MAIN_TID.get().is_some_and(|m| *m == cur);
        if on_main {
            fatal_box(
                "LogiVoltTray 发生错误",
                &format!("程序遇到内部错误，即将退出。\n\n{info}"),
            );
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 时间格式化：字段逐一映射到 YYYY-MM-DD HH:MM:SS.mmm。
    #[test]
    fn formats_local_time_fields() {
        let st = SYSTEMTIME {
            wYear: 2026,
            wMonth: 10,
            wDay: 14,
            wDayOfWeek: 3,
            wHour: 9,
            wMinute: 30,
            wSecond: 5,
            wMilliseconds: 12,
        };
        assert_eq!(format_local_time(&st), "2026-10-14 09:30:05.012");
    }

    /// 目录不存在时自动创建；追加模式可重复打开；超限旧日志被轮转清零。
    #[test]
    fn open_in_creates_dir_and_rotates_oversized_log() {
        let mut dir = std::env::temp_dir();
        dir.push(format!(
            "volttray-bootlog-test-{}-{}",
            std::process::id(),
            line!()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(open_in(&dir).is_some(), "missing dir must be auto-created");
        assert!(dir.join(LOG_FILE).is_file());
        assert!(open_in(&dir).is_some(), "append mode must re-open");
        std::fs::write(dir.join(LOG_FILE), vec![b'x'; ROTATE_BYTES as usize + 1]).unwrap();
        assert!(open_in(&dir).is_some(), "rotation must re-open cleanly");
        assert_eq!(
            std::fs::metadata(dir.join(LOG_FILE)).unwrap().len(),
            0,
            "oversized log must be rotated"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 未初始化时 path_str 退回固定文件名，弹窗文案不为空。
    #[test]
    fn path_str_falls_back_to_filename() {
        assert_eq!(path_str(), LOG_FILE);
    }
}
