//! 单实例（评审 P1-3）：命名 mutex。
//! 第二实例发现已有实例在运行后以退出码 0 退出（1s 内，无窗口、无提示）。
//!
//! E14：删除「命名激活事件」死代码——事件被 create/signal 但全仓
//! 无任何监听方（无 WaitForSingleObject/OpenEventW），信号永不被人接收。
//!
//! v0.3.22（质检复核 P0-1）：「立即重启」子实例带环境标记启动，识别后
//! 轮询等待旧实例释放互斥体（≤ [`RESTART_WAIT_TIMEOUT`]）再进主流程——
//! 修复「先 spawn 再优雅退出」与 [`acquire`] 对已存在互斥体不等待之间
//! 的竞态（旧进程关机期间新进程撞 ERROR_ALREADY_EXISTS → 弹「已在运行」
//! 后新旧双退）。

use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;

const MUTEX_NAME: &str = "Local\\LogiVoltTray.SingleInstance";

/// 重启子实例等待旧实例释放互斥体的超时上限：旧实例优雅退出路径为
/// IoCmd::Shutdown（IO 线程 recv 被命令即时打断）+ ENDSESSION_HOOK 应答
/// ≤2s + PostQuitMessage，5s 留有充足余量；超时即视为重启失败，由调用方
/// 给出明确提示（不静默消失）。
pub const RESTART_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

/// 轮询间隔：小于新实例原「100~300ms 即走到 CreateMutexW」的竞态窗口，
/// 保证旧实例一释放即可接管。
pub const RESTART_WAIT_POLL: Duration = Duration::from_millis(100);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub enum SingleInstance {
    /// 首个实例：持有 mutex 句柄直到进程退出。
    First(HANDLE),
    /// 已有实例在运行：应立即 exit(0)。
    Second,
}

/// v0.3.22（质检复核 P0-1）：重启子实例的互斥体等待结果——成功与超时
/// 分开，超时路径由调用方落日志并弹明确失败提示。
pub enum RestartAcquire {
    /// 旧实例已退出并释放互斥体，本实例接管为唯一实例。
    Acquired(HANDLE),
    /// 等待超时旧实例仍未释放互斥体。
    Timeout,
}

/// 尝试创建并持有单实例互斥体：成功返回 `Some(句柄)`；已存在（旧实例
/// 仍持有）返回 `None`，不发激活事件——是否激活由调用方按场景决定。
fn try_acquire() -> Result<Option<HANDLE>, String> {
    // SAFETY(E9): name 为 NUL 结尾宽字符缓冲、存活至调用返回；互斥体句柄成功时移交调用方持有，已存在路径不重复持有。
    unsafe {
        let name = wide(MUTEX_NAME);
        let mutex = CreateMutexW(None, true, PCWSTR::from_raw(name.as_ptr()))
            .map_err(|e| format!("CreateMutexW failed: {e}"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(mutex);
            return Ok(None);
        }
        Ok(Some(mutex))
    }
}

pub fn acquire() -> Result<SingleInstance, String> {
    match try_acquire()? {
        Some(h) => Ok(SingleInstance::First(h)),
        None => Ok(SingleInstance::Second),
    }
}

/// v0.3.22（质检复核 P0-1）：重启子实例专用——轮询等待旧实例释放
/// 互斥体再进主流程；等待期间不发激活事件、不弹「已在运行」。超时
/// 返回 [`RestartAcquire::Timeout`]，由调用方给明确失败提示。
pub fn acquire_restart_child() -> Result<RestartAcquire, String> {
    acquire_restart_child_within(RESTART_WAIT_TIMEOUT, RESTART_WAIT_POLL)
}

/// 参数化等待上限与轮询间隔，便于回归断言在不拖慢 CI 的前提下覆盖
/// 超时路径。
fn acquire_restart_child_within(wait: Duration, poll: Duration) -> Result<RestartAcquire, String> {
    let deadline = Instant::now() + wait;
    loop {
        match try_acquire()? {
            Some(h) => return Ok(RestartAcquire::Acquired(h)),
            None => {
                let remain = deadline.saturating_duration_since(Instant::now());
                if remain.is_zero() {
                    return Ok(RestartAcquire::Timeout);
                }
                std::thread::sleep(remain.min(poll));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close_raw(raw: isize) {
        // SAFETY(E9): raw 为测试自持有的有效句柄值；CloseHandle 失败忽略，不影响测试断言。
        unsafe {
            let _ = CloseHandle(HANDLE(raw as *mut core::ffi::c_void));
        }
    }

    /// 质检复核 P0-1 回归：覆盖「重启请求 → 新实例等待互斥体 → 成功
    /// 接管」与「超时未释放 → 明确失败（Timeout，调用方弹提示）」两
    /// 路径。命名互斥体是登录会话级全局对象，两条路径放在单测试函数
    /// 内串行执行，避免并行用例互相占用互斥体。
    #[test]
    fn restart_child_waits_then_takes_over_or_times_out() {
        // 路径一：旧实例持有互斥体、200ms 后释放（模拟优雅退出）——
        // 子实例必须在超时前等到并接管，而不是撞「已在运行」。
        let old = try_acquire()
            .expect("CreateMutexW 必须成功")
            .expect("测试环境应无其它实例持有互斥体");
        let raw = old.0 as isize; // 句柄以裸值移交释放线程（HANDLE 无 Drop）
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            close_raw(raw);
        });
        let started = Instant::now();
        match acquire_restart_child().expect("等待获取不应出错") {
            RestartAcquire::Acquired(h) => {
                assert!(
                    started.elapsed() >= Duration::from_millis(150),
                    "必须真的等到旧实例释放后才接管，而非立即抢占"
                );
                close_raw(h.0 as isize);
            }
            RestartAcquire::Timeout => panic!("旧实例 200ms 即释放，5s 上限内不应超时"),
        }
        releaser.join().unwrap();

        // 路径二：旧实例持有互斥体且超过等待上限仍不释放——子实例
        // 拿到 Timeout（main 据此弹「重启失败」提示并以非零码退出，
        // 失败路径可观测、不静默消失）。
        let held = try_acquire()
            .expect("CreateMutexW 必须成功")
            .expect("接管释放后互斥体应可再次获取");
        let wait = Duration::from_millis(300);
        let started = Instant::now();
        match acquire_restart_child_within(wait, Duration::from_millis(50))
            .expect("等待获取不应出错")
        {
            RestartAcquire::Acquired(h) => {
                close_raw(h.0 as isize);
                panic!("互斥体被持有时不应获取成功");
            }
            RestartAcquire::Timeout => {
                assert!(
                    started.elapsed() >= wait - Duration::from_millis(50),
                    "必须等满等待上限才判超时"
                );
            }
        }
        close_raw(held.0 as isize);
    }
}
