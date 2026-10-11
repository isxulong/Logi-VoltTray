//! volttray-app —— 应用核心层。
//!
//! 职责（蓝图契约）：配置 schema 与热重载所有权（P1-8/P1-2）、阈值去抖（M3）、
//! 诊断观测流 RingBuffer（P1-2）、帧级脱敏导出（P1-6）、快照与默认展示设备、
//! 自启动（写入边界 1）与 AUMID 快捷方式（写入边界 4，P0-5）。

pub mod aumid;
pub mod autostart;
pub mod config;
pub mod core;
pub mod debounce;
pub mod observer;

pub use config::{Config, ConfigWarning, ConfigWatcher, IconStyle};
pub use core::{guarded, AppCore, DeviceSnapshot, Snapshot, GRAY_AFTER, POLL_CHARGING};
pub use debounce::{Alert, ThresholdDebouncer, DEBOUNCE_COUNT};
pub use observer::{mask_event, mask_hex, Observer, SharedObserver, DIAG_CAPACITY};
