//! volttray-app —— 应用核心层。
//!
//! 职责（蓝图契约）：配置 schema 与热重载所有权（P1-8/P1-2）、低电追踪
//! （v0.3.0 M11/M12 单阈值 + 闪烁判定）、诊断观测流 RingBuffer（P1-2）、
//! 帧级脱敏导出（P1-6）、快照与默认展示设备、自启动（写入边界 1）与
//! AUMID 快捷方式（写入边界 4，P0-5）。

pub mod attribution;
pub mod aumid;
pub mod autostart;
pub mod config;
pub mod core;
pub mod debounce;
pub mod diag_export;
pub mod observer;
pub mod persist;
pub mod physkey;
pub mod reconnect;
pub mod redact;
pub mod settings_logic;
pub mod settings_text;
pub mod stats;
pub mod wakechannel;

pub use attribution::{
    anonymize_map, build_report_data, build_segments, event_keys, fmt_local_date, fmt_rfc3339,
    fnv1a64_hex, render_csv, render_html, render_json_events, stats_export_filename_local,
    Chart1Series, Segment, SegmentType, TypeDurations, CHART1_BUCKET_SECS,
    CHART1_DEFAULT_WINDOW_DAYS, CSV_HEADER, GAP_OVER_LIMIT_SECS, MERGE_WINDOW_SECS,
    NOTE_BELOW_WINDOW, NOTE_BOUNDARY, NOTE_CLOCK_ANOMALY, NOTE_EVIDENCE_MISSING, NOTE_GAP_OVER_7D,
    NOTE_RUN_GAP, RECENT_DAYS,
};
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
    guarded, AppCore, DeviceSnapshot, FastPollState, PollDecision, Snapshot, TimeoutOutcome,
    UpsertOutcome, GRAY_AFTER, GRAY_MIN_POLLS, NEAR_GRAY_LEEWAY, POLL_CHARGING,
    SLEEP_AFTER_TIMEOUTS, WAKE_PROBE_COOLDOWN,
};
pub use debounce::{Alert, LowBatteryTracker, DEBOUNCE_COUNT, RELEASE_HYSTERESIS};
pub use observer::{mask_event, mask_hex, Observer, SharedObserver, DIAG_CAPACITY};
pub use persist::{
    default_path as default_store_path, FlushOutcome, FlushTuning, LastKnownStore, LoadOutcome,
    PersistedDevice, FLUSH_FALLBACK, FSYNC_INTERVAL, GLOBAL_DEBOUNCE, MAX_BYTES, MAX_DEVICES,
    MAX_NAME_CHARS, PER_DEVICE_DEBOUNCE, SCHEMA_VERSION, STORE_BACKUP_NAME, STORE_DIR_NAME,
    STORE_FILE_NAME,
};
pub use physkey::{identity_confirmed, pick_promotion, repin, AliasSlot};
pub use reconnect::{
    wake_action, wake_arm_set, ChannelLiveness, DeadSlotScan, EnumTransition, Reconnect,
    SlotOutcome, SlotScanFailure, SlotScanStreak, WakeAction, DEAD_SLOT_COOLDOWN_ROUNDS,
    ENUM_BACKOFF_STEPS, HANDLE_INVALIDATE_THRESHOLD, REENUM_INTERVAL, SLOT_SCAN_REOPEN_THRESHOLD,
    WAKE_PROBE_INVALIDATE_THRESHOLD,
};
pub use stats::{
    default_dir as default_stats_dir, default_path as default_stats_path, load_events,
    load_events_from, read_source, BoundaryDevice, ReadEvent, StatsEvent, StatsFlushOutcome,
    StatsLoadOutcome, StatsStore, SOURCE_CHARGING_FAST, SOURCE_POLL, SOURCE_WAKE_FIRST_READ,
    STATS_ARCHIVE_NAME, STATS_FILE_NAME, STATS_MAX_BYTES,
};
pub use wakechannel::{channel_for_raw_path, channel_for_raw_path_classified, ChannelMatch};
