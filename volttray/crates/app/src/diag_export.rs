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

/// 诊断导出文件名前缀（v0.3.15：导出改为用户「另存为」自选位置后，
/// 默认文件名口径集中到 app 层，GUI 与测试共用一处定义）。
pub const DIAG_EXPORT_PREFIX: &str = "LogiVoltTray-diag-export-";

/// 本地时间分量（v0.3.15 第 7 项）：注入式参数，格式化逻辑跨平台可测；
/// Windows 侧由 GetLocalTime 填充（tray 层胶水），不引入 chrono。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTimeParts {
    pub year: u16,
    pub month: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
    pub millis: u16,
}

/// 导出默认文件名：`LogiVoltTray-diag-export-<YYYYMMDD>-<HHMMSS>-<mmm>.json`
/// （本地时间可读格式，v0.3.15 第 7 项；Windows 文件名不能含冒号，故用
/// `-` 分隔）。同名碰撞兜底：同一毫秒内重复导出时由「另存为」对话框的
/// OFN_OVERWRITEPROMPT 做覆盖前确认（tray 层已实现）——比追加序号简单
/// 且文件名保持可读，用户显式确认才覆盖。
/// （仅生成文件名；软件不删除、不清理任何导出文件——19:17 用户裁定：
/// 导出文件由用户自行管理。）
pub fn diag_export_filename_local(t: LocalTimeParts) -> String {
    format!(
        "{DIAG_EXPORT_PREFIX}{:04}{:02}{:02}-{:02}{:02}{:02}-{:03}.json",
        t.year, t.month, t.day, t.hour, t.minute, t.second, t.millis
    )
}

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
    /// A-3 可观测性：Raw Input hDevice→通道映射失败累计次数
    /// （v3 红队 P2-6：回看回路闭环——miss 集中于某类设备即触发
    /// channel-map 增补评估；蓝牙/有线直连系统性 miss 属预期）。
    pub channel_map_miss: u64,
    /// R-11（v0.3.17 真机终审）：写帧 / 读帧累计数（含重试；不含
    /// get_feature_report 控制传输——注册期一次性事务，口径见 tray
    /// 侧 FrameStats 注释）。仅追加，既有字段与结构不动。
    pub frame_tx: u64,
    pub frame_rx: u64,
    /// 平均写帧间隔（毫秒；无样本时为 null、序列化时省略该键）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub avg_tx_interval_ms: Option<u64>,
    /// feature_index（HID++ 帧第 3 字节，"0xXX" 字符串键）写帧分类
    /// 计数。BF-4：该字节在 HID++ 2.0 帧下为 feature_index（功能索
    /// 引，非 feature_id 功能号），HID++ 1.0 帧下为 sub_id——命名以
    /// 帧格式实际语义为准。
    pub feature_index_tx: std::collections::BTreeMap<String, u64>,
    /// feature_index 读帧分类计数（口径同上）。
    pub feature_index_rx: std::collections::BTreeMap<String, u64>,
    /// 唤醒 arm-all 装填累计数（设备台次；attribution miss 时全量装填）。
    pub arm_all_fills: u64,
    /// 唤醒成功累计数（探针轮确认设备上线）。
    pub wake_successes: u64,
    /// attribution miss 子分类：无 Raw Input 路径（no-path）。
    pub wake_miss_no_path: u64,
    /// attribution miss 子分类：零候选匹配 / 指纹不可解析（no-match；
    /// BF-3 自 ambiguous 拆出——系统性失配：BT / 有线 / 他机接收器）。
    pub wake_miss_no_match: u64,
    /// attribution miss 子分类：多候选同指纹无法裁决（ambiguous）。
    pub wake_miss_ambiguous: u64,
    /// 导出时刻（epoch 秒，与导出文件名后缀一致）。
    pub exported_at: u64,
    /// 导出程序版本（CARGO_PKG_VERSION）。
    pub app_version: String,
}

impl DiagExport {
    /// 序列化为 JSON 文本（设备名不脱敏——既有语义，旧调用形态零改动）。
    /// 字段全部可序列化，此路径不会失败；兜底返回 `"{}"`（导出永远成功）。
    pub fn to_json_string(&self) -> String {
        self.to_json_string_with(&[])
    }

    /// 序列化为 JSON 文本并对全文执行文本侧脱敏（T-02a）：
    /// `C:\Users\<名>` → `<USER>`（覆盖 USERPROFILE/APPDATA/LOCALAPPDATA
    /// 三前缀主干），设备名按出现顺序 → 「设备N」。设备名由调用方
    /// （tray 侧 export_diag）从诊断事件流 DeviceFound 收集；锁中毒时
    /// 收集为空数组，仅规则一生效（残余登记 T-02b 审计与对账跟踪）。
    pub fn to_json_string_with(&self, device_names: &[String]) -> String {
        let raw = serde_json::to_string_pretty(self).unwrap_or_else(|_| "{}".to_string());
        crate::redact::redact_text(&raw, device_names)
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

    /// v0.3.15 第 7 项：默认文件名改本地时间可读格式——
    /// `LogiVoltTray-diag-export-<YYYYMMDD>-<HHMMSS>-<mmm>.json`，
    /// 各分量零填充、Windows 文件名字符合法（无冒号）。
    #[test]
    fn export_filename_uses_readable_local_time() {
        let name = diag_export_filename_local(LocalTimeParts {
            year: 2026,
            month: 10,
            day: 16,
            hour: 21,
            minute: 29,
            second: 6,
            millis: 483,
        });
        assert_eq!(name, "LogiVoltTray-diag-export-20261016-212906-483.json");
        assert!(!name.contains(':'), "Windows 文件名不得含冒号");
    }

    /// 零填充边界：个位数月/日/时/分/秒与毫秒不足三位时补齐。
    #[test]
    fn export_filename_zero_pads_all_parts() {
        let name = diag_export_filename_local(LocalTimeParts {
            year: 2026,
            month: 1,
            day: 2,
            hour: 3,
            minute: 4,
            second: 5,
            millis: 7,
        });
        assert_eq!(name, "LogiVoltTray-diag-export-20260102-030405-007.json");
    }

    fn bundle_json(log_path: &Path) -> String {
        DiagExport {
            devices: serde_json::json!([]),
            devices_reason: None,
            log_tail: read_log_tail(log_path, LOG_TAIL_MAX_LINES),
            last_enum: None,
            last_enum_reason: None,
            channel_map_miss: 0,
            frame_tx: 0,
            frame_rx: 0,
            avg_tx_interval_ms: None,
            feature_index_tx: std::collections::BTreeMap::new(),
            feature_index_rx: std::collections::BTreeMap::new(),
            arm_all_fills: 0,
            wake_successes: 0,
            wake_miss_no_path: 0,
            wake_miss_no_match: 0,
            wake_miss_ambiguous: 0,
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
            channel_map_miss: 0,
            frame_tx: 0,
            frame_rx: 0,
            avg_tx_interval_ms: None,
            feature_index_tx: std::collections::BTreeMap::new(),
            feature_index_rx: std::collections::BTreeMap::new(),
            arm_all_fills: 0,
            wake_successes: 0,
            wake_miss_no_path: 0,
            wake_miss_no_match: 0,
            wake_miss_ambiguous: 0,
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
            channel_map_miss: 0,
            frame_tx: 0,
            frame_rx: 0,
            avg_tx_interval_ms: None,
            feature_index_tx: std::collections::BTreeMap::new(),
            feature_index_rx: std::collections::BTreeMap::new(),
            arm_all_fills: 0,
            wake_successes: 0,
            wake_miss_no_path: 0,
            wake_miss_no_match: 0,
            wake_miss_ambiguous: 0,
            exported_at: 1,
            app_version: "0.0.0-test".into(),
        }
        .to_json_string();
        assert!(!json.contains("12 34 AB CD"), "serial leaked: {json}");
        assert!(json.contains("** ** ** **"), "mask missing: {json}");
    }

    /// R-11：summary 追加六类字段正常序列化（仅追加、既有字段不动）；
    /// avg_tx_interval_ms 为 None 时该键省略。
    #[test]
    fn summary_extension_fields_serialized() {
        let mut feature_tx = std::collections::BTreeMap::new();
        feature_tx.insert("0x10".to_string(), 7u64);
        let json = DiagExport {
            devices: serde_json::json!([]),
            devices_reason: None,
            log_tail: LogTail {
                path: String::new(),
                lines: Vec::new(),
                reason: None,
            },
            last_enum: None,
            last_enum_reason: None,
            channel_map_miss: 0,
            frame_tx: 12,
            frame_rx: 9,
            avg_tx_interval_ms: Some(500),
            feature_index_tx: feature_tx,
            feature_index_rx: std::collections::BTreeMap::new(),
            arm_all_fills: 3,
            wake_successes: 2,
            wake_miss_no_path: 4,
            wake_miss_no_match: 5,
            wake_miss_ambiguous: 1,
            exported_at: 1,
            app_version: "0.0.0-test".into(),
        }
        .to_json_string();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["frame_tx"], 12);
        assert_eq!(v["frame_rx"], 9);
        assert_eq!(v["avg_tx_interval_ms"], 500);
        assert_eq!(v["feature_index_tx"]["0x10"], 7);
        assert_eq!(v["feature_index_rx"], serde_json::json!({}));
        assert_eq!(v["arm_all_fills"], 3);
        assert_eq!(v["wake_successes"], 2);
        assert_eq!(v["wake_miss_no_path"], 4);
        assert_eq!(v["wake_miss_no_match"], 5);
        assert_eq!(v["wake_miss_ambiguous"], 1);
        // avg_tx_interval_ms = None → 键省略（skip_serializing_if）。
        let v2: serde_json::Value =
            serde_json::from_str(&bundle_json(&missing_log("avg-none"))).unwrap();
        assert!(v2.get("avg_tx_interval_ms").is_none());
    }

    /// T-02b 回归测试（A 加码）：枚举 DiagExport 全部序列化字段名并断言
    /// 全文无 `C:\Users\<用户名段>` 残留——使审计结论
    /// （docs/experience/05-diag-export-field-audit.md）不随结构体/依赖
    /// 升级漂移：字段集变化时本测试失败，强制同步审计文档。
    /// 字段值埋入单反斜杠路径（serde 输出即 JSON 转义形态，redact 规则一
    /// 的真实处理形态）；tag 原文断言锁定「维持现状」结论（防误伤排障信息）。
    #[test]
    fn t02b_all_fields_enumerated_and_no_user_path_leak() {
        let secret_user = "zhangsan_audit";
        let device_name = "MX Master 3S Wireless Mouse (audit)";
        let user_path = format!(r"C:\Users\{}\AppData\Roaming\LogiVoltTray", secret_user);
        let devices = serde_json::json!([
            {
                "seq": 1,
                "ts_ms": 1,
                "layer": "app",
                "type": "device_found",
                "id": "046D|7&31a4c5e6&0&0000#3",
                "name": device_name,
            },
            {
                "seq": 2,
                "ts_ms": 2,
                "layer": "transport",
                "type": "io_error",
                "message": format!("hid open failed at {user_path}"),
            },
        ]);
        let export = DiagExport {
            devices,
            devices_reason: Some(format!("path probe {user_path}")),
            log_tail: LogTail {
                path: user_path.clone(),
                lines: vec![format!("persist battery to {user_path} failed")],
                reason: Some(format!("log read failed at {user_path}")),
            },
            last_enum: Some(EnumSnapshot {
                at_ms: 1_760_000_012_345,
                forced: true,
                candidate_channels: 1,
                channels: vec![ChannelEnumResult {
                    key: "C52B|7&31a4c5e6&0&0000".into(),
                    pid: "0xC52B".into(),
                    tag: "7&31a4c5e6&0&0000".into(),
                    open: "failed".into(),
                    error: Some(format!("hid open error: {user_path}")),
                }],
                registered_slots: 0,
                error: Some(format!("enum failed: {user_path}")),
            }),
            last_enum_reason: Some(format!("enum snapshot lock poisoned at {user_path}")),
            channel_map_miss: 0,
            frame_tx: 0,
            frame_rx: 0,
            avg_tx_interval_ms: Some(12),
            feature_index_tx: [("0x01".to_string(), 1u64)].into_iter().collect(),
            feature_index_rx: [("0x01".to_string(), 1u64)].into_iter().collect(),
            arm_all_fills: 0,
            wake_successes: 0,
            wake_miss_no_path: 0,
            wake_miss_no_match: 0,
            wake_miss_ambiguous: 0,
            exported_at: 1_760_000_000,
            app_version: "0.0.0-audit".into(),
        };
        let device_names = vec![device_name.to_string()];
        let json = export.to_json_string_with(&device_names);

        // ① 假用户名零残留（全文、含 JSON 转义形态）。
        assert!(!json.contains(secret_user), "用户名残留：{json}");
        // ② 设备名零残留。
        assert!(!json.contains(device_name), "设备名残留：{json}");
        // ③ <USER> 替换生效。
        assert!(json.contains("<USER>"), "规则一未生效：{json}");
        // ④ 字段名枚举：顶层键与审计文档 18 字段清单严格一致；
        //    结构体字段集漂移在此失败，强制同步 05 审计文档。
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
        keys.sort();
        let mut expected: Vec<&str> = [
            "app_version",
            "arm_all_fills",
            "avg_tx_interval_ms",
            "channel_map_miss",
            "devices",
            "devices_reason",
            "exported_at",
            "feature_index_rx",
            "feature_index_tx",
            "frame_rx",
            "frame_tx",
            "last_enum",
            "last_enum_reason",
            "log_tail",
            "wake_miss_ambiguous",
            "wake_miss_no_match",
            "wake_miss_no_path",
            "wake_successes",
        ]
        .into();
        expected.sort();
        assert_eq!(
            keys, expected,
            "DiagExport 序列化字段集与 05 审计文档清单不一致：{keys:?} vs {expected:?}"
        );
        // ⑤ 锁定 tag 维持现状（05 文档第三节①处置结论）：实例指纹
        //    不脱敏，防未来误加规则静默破坏排障信息。
        assert!(json.contains("7&31a4c5e6&0&0000"), "tag 原文应保留：{json}");
    }
}
