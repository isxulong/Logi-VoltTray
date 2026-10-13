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
    brighten, contrast_ok, contrast_ratio, flash_rgb, font_tier_for_percent, normalize_font_tiers,
    normalize_poll_secs, Config, ConfigWarning, ConfigWatcher, IconBackground, IconFontColor,
    IconShape, IconStyle, RgbColor, CONTRAST_FLOOR, DEFAULT_FONT_TIER_COLORS,
    DEFAULT_FONT_TIER_THRESHOLDS, DEFAULT_LOW_THRESHOLD, DEFAULT_POLL_SECS, FONT_TIER_MAX_TIERS,
    FONT_TIER_MIN_TIERS, FONT_TIER_THRESHOLD_MAX, FONT_TIER_THRESHOLD_MIN, ICON_BG_SCALE_MAX,
    ICON_BG_SCALE_MIN, ICON_TEXT_SCALE_MAX, ICON_TEXT_SCALE_MIN, LOW_THRESHOLD_MAX,
    LOW_THRESHOLD_MIN, POLL_MAX_SECS, POLL_MIN_SECS, POLL_TIERS_SECS, TIER_PAD_PALETTE,
};
pub use core::{
    guarded, AppCore, DeviceSnapshot, PollDecision, Snapshot, TimeoutOutcome, GRAY_AFTER,
    POLL_CHARGING, SLEEP_AFTER_TIMEOUTS, WAKE_PROBE_COOLDOWN,
};
pub use debounce::{Alert, LowBatteryTracker, DEBOUNCE_COUNT, RELEASE_HYSTERESIS};
pub use observer::{mask_event, mask_hex, Observer, SharedObserver, DIAG_CAPACITY};
