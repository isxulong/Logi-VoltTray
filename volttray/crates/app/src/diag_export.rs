//! 诊断导出增强（v0.3.12，D-69）：导出 JSON 从裸事件数组重构为带上下文
//! 的对象。真机教训（v0.3.11 复盘后）：重启后识别失败时导出内容是空数组
//! `[]`——零设备场景导出毫无信息量，远程排障还要二次索取 .log 文件。
//!
//! 导出对象字段（**破坏性变更**，向后兼容说明见 RELEASES.md）：
//!
//! - `devices`：原导出数组（诊断事件流，脱敏逻辑不变）——现状逻辑不动；
//! - `log_tail`：`logi-volttray.log` 最后 [`LOG_TAIL_MAX_LINES`] 行 +
//!   说明字段（文件不存在/不可读 → 空数组 + reason，绝不使导出失败）；
//! - `last_enum`：最近一次枚举快照（候选通道数、各通道 open 结果、注册
//!   槽位数），从内存状态取，尚未枚举过为 `null`；
//! - `exported_at` / `app_version`：导出时刻（epoch 秒）与导出程序版本。
//!
//! 硬约束：**导出永远成功**——任何子项失败降级为空 + reason
//! （v0.3.2 的 os error 3 教训：诊断手段自身绝不允许阻断诊断）。

use std::path::Path;

use serde::Serialize;

/// 日志尾部行数上限（约 200 行：1MB 轮转上限内的近期上下文，够定位
/// 「重启后识别失败」这类会话级问题，又不至于把导出文件撑爆）。
pub const LOG_TAIL_MAX_LINES: usize = 200;

/// 日志尾部（`lines` 为空时 `reason` 说明原因）。
#[derive(Debug, Clone, Serialize)]
pub struct LogTail {
    /// 被读取的日志文件路径（远程排障时确认读的是哪个文件）。
    pub path: String,
    /// 最后 N 行（原顺序）。
    pub lines: Vec<String>,
    /// `lines` 非完整尾部时的说明（文件不存在 / 不可读 / 空文件）。
    pub reason: Option<String>,
}

/// 单个候选通道在本轮枚举中的 open 结果。
#[derive(Debug, Clone, Serialize)]
pub struct ChannelEnumResult {
    /// 通道 key（与重枚举裁决用 key 一致）。
    pub key: String,
    /// PID，十六进制字符串（与日志口径一致，如 "0xC533"）。
    pub pid: String,
    /// 接收器 tag（路径稳定标识，transport::ChannelInfo::tag 为 String）。
    pub tag: String,
    /// open 结果：`"ok"`（新通道本轮打开成功）/ `"failed"`（打开失败）/
    /// `"cached"`（已有通道沿用缓存句柄）/ `"reopened"`（强制轮或扫槽
    /// 连败触发重开成功）/ `"cached-reopen-failed"`（重开失败但保留缓存句柄）。
    pub open: String,
    /// `open = "failed" | "cached-reopen-failed"` 时的错误原文。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 最近一次枚举快照（IO 线程每轮枚举后更新，导出时从内存读取）。
#[derive(Debug, Clone, Serialize)]
pub struct EnumSnapshot {
    /// 快照时刻（epoch 毫秒）。
    pub at_ms: u64,
    /// 本轮是否为强制重枚举（「立即刷新」/ 句柄失效触发）。
    pub forced: bool,
    /// 候选通道数。
    pub candidate_channels: usize,
    /// 各候选通道的 open 结果。
    pub channels: Vec<ChannelEnumResult>,
    /// 本轮枚举+槽位扫描后的注册槽位总数。
    pub registered_slots: usize,
    /// 枚举本身失败的原因（如 HidApi 初始化失败），正常为 null。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 完整导出对象。`*_reason` 仅在对应子项被降级时出现。
#[derive(Debug, Serialize)]
pub struct DiagExport {
    /// 原导出数组（诊断事件流，已按 mask 脱敏）——现状逻辑不动。
    pub devices: serde_json::Value,
    /// devices 降级原因（如 observer 锁中毒）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub devices_reason: Option<String>,
    /// 日志尾部。
    pub log_tail: LogTail,
    /// 最近一次枚举快照；尚未枚举过为 null。
    pub last_enum: Option<EnumSnapshot>,
    /// last_enum 降级原因（如快照锁中毒）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_enum_reason: Option<String>,
    /// 导出时刻（epoch 秒，与导出文件名后缀一致）。
    pub exported_at: u64,
    /// 导出程序版本（CARGO_PKG_VERSION）。
    pub app_version: String,
}

impl DiagExport {
    /// 序列化为 JSON 文本。字段全部可序列化，此路径不会失败；
    /// 兜底返回 `"{}"`（导出永远成功）。
    pub fn to_json_string(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string())
    }
}

/// 读取日志尾部。文件不存在/不可读 → 空数组 + reason（**绝不返回 Err**）。
pub fn read_log_tail(path: &Path, max_lines: usize) -> LogTail {
    let path_str = path.to_string_lossy().into_owned();
    match std::fs::read_to_string(path) {
        Ok(content) => {
            let all: Vec<&str> = content.lines().collect();
            if all.is_empty() {
                LogTail {
                    path: path_str,
                    lines: Vec::new(),
                    reason: Some("log file is empty".to_string()),
                }
            } else {
                let start = all.len().saturating_sub(max_lines);
                LogTail {
                    path: path_str,
                    lines: all[start..].iter().map(|s| s.to_string()).collect(),
                    reason: None,
                }
            }
        }
        Err(e) => {
            // 文件不存在是常态（日志目录尚未创建/被清理），与真实 I/O
            // 错误分开说明；两者都不允许让导出失败。
            let reason = if e.kind() == std::io::ErrorKind::NotFound {
                format!("log file not found: {e}")
            } else {
                format!("log read failed: {e}")
            };
            LogTail {
                path: path_str,
                lines: Vec::new(),
                reason: Some(reason),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "volttray-diag-export-test-{}-{}",
            std::process::id(),
            tag
        ));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    fn missing_log(tag: &str) -> PathBuf {
        temp_dir(tag).join("logi-volttray.log")
    }

    fn bundle_json(log_path: &Path) -> String {
        DiagExport {
            devices: serde_json::json!([]),
            devices_reason: None,
            log_tail: read_log_tail(log_path, LOG_TAIL_MAX_LINES),
            last_enum: None,
            last_enum_reason: None,
            exported_at: 1_760_000_000,
            app_version: "0.0.0-test".to_string(),
        }
        .to_json_string()
    }

    /// 三态之一：日志文件不存在 → 空数组 + 说明字段，导出仍成功。
    #[test]
    fn log_tail_missing_file_gives_empty_lines_and_reason() {
        let json = bundle_json(&missing_log("missing"));
        let v: serde_json::Value = serde_json::from_str(&json).expect("export must be valid json");
        assert!(
            v["devices"].is_array(),
            "devices must stay an array: {json}"
        );
        assert_eq!(v["log_tail"]["lines"].as_array().map(Vec::len), Some(0));
        let reason = v["log_tail"]["reason"].as_str().unwrap_or_default();
        assert!(reason.contains("not found"), "reason expected: {json}");
        assert_eq!(v["last_enum"], serde_json::json!(null));
        assert_eq!(v["exported_at"], 1_760_000_000);
        assert_eq!(v["app_version"], "0.0.0-test");
    }

    /// 三态之二：空日志文件 → 空数组 + 空文件说明（与「不存在」可区分）。
    #[test]
    fn log_tail_empty_file_gives_empty_lines_and_reason() {
        let dir = temp_dir("empty");
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("logi-volttray.log");
        std::fs::write(&log, "").unwrap();
        let json = bundle_json(&log);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["log_tail"]["lines"].as_array().map(Vec::len), Some(0));
        assert_eq!(
            v["log_tail"]["reason"].as_str(),
            Some("log file is empty"),
            "empty file must be distinguishable from missing: {json}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 三态之三：正常日志 → 取最后 LOG_TAIL_MAX_LINES 行且保持原顺序；
    /// 不足上限时全量返回。
    #[test]
    fn log_tail_normal_file_returns_last_lines_in_order() {
        let dir = temp_dir("normal");
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("logi-volttray.log");
        let body: String = (0..250).map(|i| format!("line-{i}\n")).collect();
        std::fs::write(&log, body).unwrap();
        let json = bundle_json(&log);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let lines = v["log_tail"]["lines"].as_array().unwrap();
        assert_eq!(lines.len(), LOG_TAIL_MAX_LINES, "tail must cap at 200");
        assert_eq!(lines[0].as_str().unwrap(), "line-50", "must keep order");
        assert_eq!(lines[199].as_str().unwrap(), "line-249");
        assert_eq!(v["log_tail"]["reason"], serde_json::json!(null));

        // 不足上限：全量、有序。
        std::fs::write(&log, "a\nb\nc\n").unwrap();
        let v: serde_json::Value =
            serde_json::from_str(&bundle_json(&log)).expect("re-export must succeed");
        let lines = v["log_tail"]["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].as_str().unwrap(), "a");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 子项失败降级：日志路径是一个目录（读取必然失败）→ 空数组 + reason，
    /// 导出整体仍是合法 JSON 对象。
    #[test]
    fn log_tail_unreadable_path_degrades_to_reason() {
        let dir = temp_dir("unreadable");
        std::fs::create_dir_all(&dir).unwrap();
        let json = bundle_json(&dir); // 目录当日志路径
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["log_tail"]["lines"].as_array().map(Vec::len), Some(0));
        assert!(
            v["log_tail"]["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("read failed"),
            "io error reason expected: {json}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 导出永远成功 + last_enum 两种形态：null（未枚举）与快照对象。
    #[test]
    fn last_enum_null_then_snapshot_roundtrip() {
        let dir = temp_dir("enum");
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("logi-volttray.log");
        std::fs::write(&log, "hello\n").unwrap();

        let null_case = bundle_json(&log);
        let v: serde_json::Value = serde_json::from_str(&null_case).unwrap();
        assert_eq!(v["last_enum"], serde_json::json!(null));

        let snap = EnumSnapshot {
            at_ms: 1_760_000_012_345,
            forced: true,
            candidate_channels: 2,
            channels: vec![
                ChannelEnumResult {
                    key: "usb:1234:C533".into(),
                    pid: "0xC533".into(),
                    tag: "1".to_string(),
                    open: "cached".into(),
                    error: None,
                },
                ChannelEnumResult {
                    key: "usb:5678:C52B".into(),
                    pid: "0xC52B".into(),
                    tag: "2".to_string(),
                    open: "failed".into(),
                    error: Some("hid open error".into()),
                },
            ],
            registered_slots: 0,
            error: None,
        };
        let json = DiagExport {
            devices: serde_json::json!([]),
            devices_reason: Some("observer lock poisoned".into()),
            log_tail: read_log_tail(&log, LOG_TAIL_MAX_LINES),
            last_enum: Some(snap),
            last_enum_reason: None,
            exported_at: 1_760_000_000,
            app_version: "0.0.0-test".into(),
        }
        .to_json_string();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["last_enum"]["candidate_channels"], 2);
        assert_eq!(v["last_enum"]["registered_slots"], 0);
        assert_eq!(v["last_enum"]["forced"], true);
        assert_eq!(v["last_enum"]["channels"][0]["open"], "cached");
        assert_eq!(v["last_enum"]["channels"][1]["open"], "failed");
        assert_eq!(v["last_enum"]["channels"][1]["error"], "hid open error");
        // 未置 error 的通道不出现 error 字段（skip_serializing_if）。
        assert!(v["last_enum"]["channels"][0].get("error").is_none());
        assert_eq!(v["devices_reason"], "observer lock poisoned");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// devices 承接 Observer 事件流且脱敏逻辑不动（P1-6 回归）：
    /// 导出对象里的 devices 不含真实序列号字节。
    #[test]
    fn devices_keeps_masked_event_stream() {
        use volttray_transport::events::{DiagEvent, DiagKind, Layer};

        let mut obs = crate::observer::Observer::new();
        obs.push(DiagEvent::new(
            Layer::Transport,
            DiagKind::FrameRx {
                hex: "10 01 07 01 12 34 AB CD".into(),
                feature_id: Some(7),
            },
        ));
        let json = DiagExport {
            devices: obs.export_value(true),
            devices_reason: None,
            log_tail: LogTail {
                path: String::new(),
                lines: Vec::new(),
                reason: Some("log file not found".into()),
            },
            last_enum: None,
            last_enum_reason: None,
            exported_at: 1,
            app_version: "0.0.0-test".into(),
        }
        .to_json_string();
        assert!(!json.contains("12 34 AB CD"), "serial leaked: {json}");
        assert!(json.contains("** ** ** **"), "mask missing: {json}");
    }
}
