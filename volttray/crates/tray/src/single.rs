//! 单实例（评审 P1-3）：命名 mutex + 命名事件。
//! 第二实例激活第一实例后以退出码 0 退出（1s 内，无窗口、无提示）。

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Threading::{CreateEventW, CreateMutexW, SetEvent};

const MUTEX_NAME: &str = "Local\\LogiVoltTray.SingleInstance";
const EVENT_NAME: &str = "Local\\LogiVoltTray.Activate";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub enum SingleInstance {
    /// 首个实例：持有 mutex 句柄直到进程退出。
    First(HANDLE),
    /// 已有实例在运行：已发出激活事件，应立即 exit(0)。
    Second,
}

pub fn acquire() -> Result<SingleInstance, String> {
    unsafe {
        let name = wide(MUTEX_NAME);
        let mutex = CreateMutexW(None, true, PCWSTR::from_raw(name.as_ptr()))
            .map_err(|e| format!("CreateMutexW failed: {e}"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let ev_name = wide(EVENT_NAME);
            if let Ok(ev) = CreateEventW(None, false, false, PCWSTR::from_raw(ev_name.as_ptr())) {
                let _ = SetEvent(ev);
                let _ = CloseHandle(ev);
            }
            let _ = CloseHandle(mutex);
            return Ok(SingleInstance::Second);
        }
        Ok(SingleInstance::First(mutex))
    }
}
