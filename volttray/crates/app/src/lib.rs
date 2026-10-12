//! volttray-app —— 应用核心层。
//!
//! 职责（蓝图契约）：配置 schema 与热重载所有权（P1-8/P1-2）、低电追踪
//! （v0.3.0 M11/M12 单阈值 + 闪烁判定）、诊断观测流 RingBuffer（P1-2）、
//! 帧级脱敏导出（P1-6）、快照与默认展示设备、自启动（写入边界 1）与
//! AUMID 快捷方式（写入边界 4，P0-5）。

pub mod aumid;
pub mod autostart;
pub mod config;
pub mod core;
pub mod debounce;
pub mod observer;

pub use config::{
    brighten, contrast_ok, contrast_ratio, Config, ConfigWarning, ConfigWatcher,
    IconBackground, IconFontColor, IconShape, IconStyle, RgbColor, CONTRAST_FLOOR,
    DEFAULT_LOW_THRESHOLD, DEFAULT_POLL_SECS, LOW_THRESHOLD_MAX, LOW_THRESHOLD_MIN,
    POLL_MAX_SECS, POLL_MIN_SECS,
};
pub use core::{guarded, AppCore, DeviceSnapshot, Snapshot, GRAY_AFTER, POLL_CHARGING};
pub use debounce::{
    Alert, LowBatteryTracker, DEBOUNCE_COUNT, RELEASE_HYSTERESIS,
};
pub use observer::{mask_event, mask_hex, Observer, SharedObserver, DIAG_CAPACITY};
