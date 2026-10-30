//! 自描述导出 manifest（组5 N-12② · 5a）：编译期静态常量——字段字典
//! ＋生效脱敏规则 id 集（与 `redact::REDACTION_RULES` 同 registry）＋
//! schema 版本＋app 版本＋生成时间。
//!
//! D-2 声明：诊断面 33h 日志时间线字段已在 ST-DOC1 G-34 登记，本组不
//! 重复立项；本模块只承担统计面（stats JSON / CSV / HTML volttray-data）
//! 与诊断导出 JSON 的 manifest 落点。
//!
//! 落点（四件套，同一 `manifest_json` 单点生成）：
//! - 统计 JSON 顶层 `manifest` 键（`attribution::render_json_events`）；
//! - HTML volttray-data 顶层 `manifest` 键（`attribution::build_report_data`）；
//! - CSV BOM 之后、表头之前的 `#` 注释块（`attribution::render_csv`；
//!   `tools/recheck.py` 同 PR 适配：跳过 `#` 行后再字节对账）；
//! - 诊断导出 JSON `manifest` 字段（`diag_export::DiagExport`）。
//!
//! BvA-6：manifest 只写规则名，不写采集时区偏移值等可定位个人的值
//! （负面清单有钉板测试）。
//!
//! 回滚（BvA 决议）：纯增量——删除本模块与四处接线即回滚，不配配置键。
//!
//! 复核二 P1-2（2026-10-31 修订批）：诊断导出 JSON 本体（DiagExport）
//! 顶层字段逐字段口径字典 `DIAG_EXPORT_TOP_FIELDS` 同批纳入本模块并随
//! `manifest_json` 输出（诊断 JSON 内嵌 manifest 即携带），钉板测试
//! 断言字典键集合与 DiagExport 实际序列化字段集严格相等防漂移。

use crate::redact::REDACTION_RULES;

/// manifest schema 版本：结构任何不兼容变更 +1（BvA-8：下游按
/// schema_version 判读，禁止按字段存在性硬解析）。
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// 统计 JSON 顶层字段字典（防漂移：`render_json_events` 键集合钉板
/// 对账，新增字段必须同步登记于此）。
pub const STATS_JSON_TOP_FIELDS: &[&str] = &["events", "exported_at", "manifest", "version"];

/// CSV 列字典（与 `attribution::CSV_HEADER` 一一对应，钉板测试断言
/// 逐字一致——两处任一改动都会红，强制同步登记）。
pub const CSV_COLUMNS: &[&str] = &[
    "device_key",
    "display_name",
    "segment_type",
    "start_ts",
    "end_ts",
    "duration_min",
    "start_pct",
    "end_pct",
    "delta_pct",
    "rate_pct_per_hour",
    "note",
];

/// HTML volttray-data 顶层字段字典（防漂移：`build_report_data` 键
/// 集合钉板对账）。
pub const HTML_DATA_TOP_FIELDS: &[&str] = &[
    "activity_gap_note",
    "avg_estimate",
    "chart1",
    "chart3",
    "daily_cards",
    "devices",
    "first_screen_kpis",
    "manifest",
    "meta",
    "run_spans_by_day",
    "segments",
    "week_over_week",
];

/// 诊断导出 JSON（`diag_export::DiagExport`）顶层字段逐字段字典
/// （复核二 P1-2）：(字段名, 口径说明) 对，说明口径可机器化复用
/// docs/experience/05-diag-export-field-audit.md 审计表（字段集口径
/// 以本字典为唯一登记位）。防漂移：钉板测试断言本字典键集合与
/// DiagExport 实际序列化顶层字段集严格相等——结构体新增/删除字段
/// 而漏登记即红。诊断 JSON 内嵌 manifest 落点携带本字典，零背景
/// 读者拿到导出文件即可逐字段读懂口径，无需回源码。
pub const DIAG_EXPORT_TOP_FIELDS: &[(&str, &str)] = &[
    ("app_version", "导出程序版本号"),
    ("arm_all_fills", "唤醒 arm-all 装填累计（设备台次，per-run；归因 miss 时全量装填）"),
    ("avg_tx_interval_ms", "平均写帧间隔（毫秒）；无样本时该键省略"),
    ("channel_map_miss", "Raw Input hDevice 到通道映射失败累计次数（per-run；蓝牙/有线直连系统性 miss 属预期）"),
    ("devices", "诊断事件流数组（帧 hex 已按敏感偏移遮蔽；observer 环形缓冲导出，容量见 DIAG_CAPACITY）"),
    ("devices_reason", "devices 降级原因（如 observer 锁中毒）；正常时该键省略"),
    ("exported_at", "导出时刻（epoch 秒，与导出文件名后缀一致）"),
    ("feature_index_rx", "读帧按 feature_index（HID++ 帧第 3 字节，形如 0xXX）分类计数"),
    ("feature_index_tx", "写帧按 feature_index 分类计数（口径同 feature_index_rx）"),
    ("frame_rx", "读帧累计数（per-run，含重试；不含注册期一次性控制传输）"),
    ("frame_tx", "写帧累计数（口径同 frame_rx）"),
    ("guaranteed_read_attempts", "无条件保底读数尝试次数（per-run；每设备每 4 小时至多 1 次，不被每日封顶拦截、不计入试探额度）"),
    ("last_enum", "最近一次枚举快照（at_ms/forced/candidate_channels/channels/registered_slots/error 嵌套）；尚未枚举过为 null"),
    ("last_enum_reason", "last_enum 降级原因（如快照锁中毒）；正常时该键省略"),
    ("log_tail", "日志尾部（path/lines/reason）；lines 非完整尾部时 reason 说明原因（文件不存在/空文件/读失败）"),
    ("manifest", "自描述 manifest（本字典所在）：schema 版本＋生效脱敏规则 id 集＋各导出形态字段字典"),
    ("poll_round_last_duration_ms", "最近一轮整轮耗时（毫秒，轮起止墙钟差口径，D-98）"),
    ("poll_round_max_duration_ms", "运行内最大整轮耗时（毫秒，口径同 poll_round_last_duration_ms）"),
    ("poll_rounds_total", "整轮轮序号累计（per-run；仅整轮，探测轮不计）"),
    ("poll_skipped_dead_slot", "扫槽被死槽冷却（连续 K 轮握手无应答）跳过的次数（per-run）"),
    ("poll_skipped_ff_cache", "轮询/扫槽被 0xFF run 内负缓存挡下的次数（per-run）"),
    ("poll_skipped_sleep", "轮询因设备休眠且无唤醒资格被挡下的次数（per-run；休眠零查询的常态）"),
    ("probe_backoff_skips", "因退避窗口未到期（30 秒起乘 2 倍增、1800 秒封顶）被抑制的唤醒试探次数（per-run）"),
    ("probe_cap_daily_hits", "因每日封顶（单设备滚动 24 小时硬顶 96 次）被抑制的唤醒试探次数（per-run）"),
    ("run_started_at", "运行起点时间戳（ISO 8601 UTC）；frame_tx 速率等派生指标的时间分母"),
    ("slot_outcome_error", "各槽查询结果累计（per-run）：错误（DeviceGone / io / 硬失败）"),
    ("slot_outcome_success", "各槽查询结果累计（per-run）：查询成功"),
    ("slot_outcome_timeout", "各槽查询结果累计（per-run）：超时（休眠路径）"),
    ("slot_query_charging", "已执行查询的档位分布（per-run）：充电 15 秒档"),
    ("slot_query_normal", "已执行查询的档位分布（per-run）：常规档"),
    ("wake_miss_ambiguous", "唤醒归因 miss 分桶（per-run）：多候选同指纹且绑定表消歧失败，歧义即 miss"),
    ("wake_miss_no_fingerprint", "唤醒归因 miss 分桶（per-run）：设备指纹缺失或畸形，未进入匹配"),
    ("wake_miss_no_path", "唤醒归因 miss 分桶（per-run）：无 Raw Input 路径"),
    ("wake_miss_zero_match", "唤醒归因 miss 分桶（per-run）：指纹可解析但本机接收器候选无一匹配（含候选集非空但指纹全不中；并非候选集为空）"),
    ("wake_recovery_via_reenum", "唤醒信号窗口内经整轮重枚举恢复注册的累计数（per-run；与探测直接命中区分）"),
    ("wake_success_hit_exact", "第一级精确指纹归因成功次数（per-run）"),
    ("wake_success_hit_relaxed", "第二级放宽/绑定表消歧归因成功次数（per-run）"),
    ("wake_successes", "唤醒成功累计数（per-run；等于 wake_success_hit_exact 加 wake_success_hit_relaxed 加 arm-all 成功）"),
];

/// 知情声明（逐字段处置留痕；纯文本，不含任何外链 token 与时区偏移
/// 值——HTML 零外链扫描与 BvA-6 双约束，均有钉板）。
pub const MANIFEST_NOTES: &[&str] = &[
    "run_start/run_stop 秒级时间线保留（U-2C 裁决原文：精度不动）",
    "HTML 首末时刻字段保留（G-34 已登记；剥离/降精度挂 U-2 拍板，本批不落地）",
    "display_name 型号默认保留（U-2C）；anonymize=on 时按导出会话映射为设备N（C-3：会话内稳定、不承诺跨导出稳定）",
    "activity_buckets 逐分钟活动位图默认不导出（组4 N-08② export_activity_bitmap 默认关，与 anonymize 解耦；勾选后随事件流并入）",
    "manifest 只写规则名，不含采集时区偏移值（BvA-6）",
    "诊断导出计数器族（wake_*/probe_*/poll_*/slot_*）均为 per-run 累计：进程启动清零、跨运行不可加总",
];

/// 生成 manifest JSON 值。`generated_at` 为导出时刻 epoch 秒（调用侧
/// 注入，不在此取系统时间——保持纯函数可钉板）。
pub fn manifest_json(generated_at: i64) -> serde_json::Value {
    let rules: Vec<serde_json::Value> = REDACTION_RULES
        .iter()
        .map(|r| serde_json::json!({ "id": r.id, "summary": r.summary }))
        .collect();
    // 复核二 P1-2：诊断导出顶层字段逐字段字典随 manifest 输出——诊断
    // JSON 的 manifest 落点内嵌本字典，零背景读者无需回源码即可读懂
    // wake_miss_* 四分桶、probe_*、slot_outcome_* 等计数器口径。
    let diag_fields: serde_json::Map<String, serde_json::Value> = DIAG_EXPORT_TOP_FIELDS
        .iter()
        .map(|(k, v)| (k.to_string(), serde_json::json!(v)))
        .collect();
    serde_json::json!({
        "schema_version": MANIFEST_SCHEMA_VERSION,
        "app_version": env!("CARGO_PKG_VERSION"),
        "generated_at": generated_at,
        "fields": {
            "stats_json": STATS_JSON_TOP_FIELDS,
            "csv_columns": CSV_COLUMNS,
            "html_data": HTML_DATA_TOP_FIELDS,
            "diag_export": diag_fields,
        },
        "redaction_rules": rules,
        "notes": MANIFEST_NOTES,
    })
}

/// CSV `#` 注释块（BOM 之后、表头之前；每行 `# key=value`）。
/// 注释块与 `manifest_json` 同源同口径；完整字段字典见 JSON/HTML 落点
/// 与 volttray/docs/stats-export-field-audit.md。
pub fn manifest_csv_comments(generated_at: i64) -> String {
    let ids: Vec<&str> = REDACTION_RULES.iter().map(|r| r.id).collect();
    let mut out = String::new();
    out.push_str(&format!(
        "# manifest.schema_version={MANIFEST_SCHEMA_VERSION}\n"
    ));
    out.push_str(&format!(
        "# manifest.app_version={}\n",
        env!("CARGO_PKG_VERSION")
    ));
    out.push_str(&format!("# manifest.generated_at={generated_at}\n"));
    out.push_str(&format!("# manifest.redaction_rules={}\n", ids.join(",")));
    out.push_str("# manifest.notes=run_start/run_stop 秒级保留(U-2C);型号默认保留,anonymize=on 映射设备N(C-3);不含采集时区偏移值(BvA-6)\n");
    out.push_str("# manifest.audit=volttray/docs/stats-export-field-audit.md\n");
    out.push_str("# manifest.diag_audit=docs/experience/05-diag-export-field-audit.md（诊断导出逐字段口径另见诊断 JSON manifest.fields.diag_export）\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// B 要素钉板：schema 版本逐字钉死，变更须连测试一起改（强制留痕）。
    #[test]
    fn manifest_schema_version_pinned() {
        assert_eq!(MANIFEST_SCHEMA_VERSION, 1);
    }

    #[test]
    fn manifest_json_verbatim_pin() {
        let m = manifest_json(1_760_000_000);
        assert_eq!(m["schema_version"], 1);
        assert_eq!(m["app_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(m["generated_at"], 1_760_000_000);
        // 规则 id 集与 redact registry 同源同序。
        let ids: Vec<&str> = m["redaction_rules"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            vec!["R1-user-path", "R2-device-name", "R3-stats-display-name"]
        );
        // 字段字典原样透出。
        assert_eq!(m["fields"]["csv_columns"].as_array().unwrap().len(), 11);
        assert_eq!(
            m["fields"]["stats_json"].as_array().unwrap().len(),
            STATS_JSON_TOP_FIELDS.len()
        );
        assert_eq!(
            m["fields"]["html_data"].as_array().unwrap().len(),
            HTML_DATA_TOP_FIELDS.len()
        );
        assert_eq!(m["notes"].as_array().unwrap().len(), MANIFEST_NOTES.len());
    }

    /// BvA-6 负面清单：manifest 全文（JSON 与 CSV 注释块）不得含采集
    /// 时区偏移值等可定位个人的值。
    #[test]
    fn manifest_negative_list_no_tz_offset() {
        let s = manifest_json(1_760_000_000).to_string();
        let c = manifest_csv_comments(1_760_000_000);
        for bad in ["+08:00", "-08:00", "+0800", "-0500", "tz_offset", "Asia/"] {
            assert!(!s.contains(bad), "manifest JSON 含负面清单值: {bad}");
            assert!(!c.contains(bad), "CSV 注释块含负面清单值: {bad}");
        }
    }

    /// HTML 零外链扫描（recheck.py check_html）同款 token：manifest 注入
    /// volttray-data 后不得误触红线。
    #[test]
    fn manifest_avoids_html_external_link_tokens() {
        let s = manifest_json(1).to_string();
        for bad in [
            "http://", "https://", "<link", "<img", "src=", "url(", "@import",
        ] {
            assert!(!s.contains(bad), "manifest 含外链 token: {bad}");
        }
    }

    #[test]
    fn csv_comment_block_shape() {
        let c = manifest_csv_comments(42);
        for line in c.lines() {
            assert!(line.starts_with("# "), "注释行须以 `# ` 开头: {line}");
        }
        assert!(c.contains("# manifest.schema_version=1\n"));
        assert!(c.contains("# manifest.generated_at=42\n"));
        assert!(c.contains(
            "# manifest.redaction_rules=R1-user-path,R2-device-name,R3-stats-display-name\n"
        ));
    }

    /// CSV 列字典与 CSV_HEADER 逐字一致（A 要素防漂移）。
    #[test]
    fn csv_columns_match_csv_header() {
        let header_cols: Vec<&str> = crate::attribution::CSV_HEADER.split(',').collect();
        assert_eq!(header_cols, CSV_COLUMNS);
    }

    /// 复核二 P1-2 防漂移钉板：DIAG_EXPORT_TOP_FIELDS 键集合与
    /// DiagExport 实际序列化顶层字段集严格相等（含全部
    /// skip_serializing_if 可选字段置 Some 的形态）——结构体字段集
    /// 漂移而字典未同步登记即红，强制双向同步。
    #[test]
    fn diag_export_field_dict_matches_serialized_keys() {
        use crate::diag_export::{DiagExport, EnumSnapshot, LogTail};
        let export = DiagExport {
            devices: serde_json::json!([]),
            devices_reason: Some("x".into()),
            log_tail: LogTail {
                path: String::new(),
                lines: Vec::new(),
                reason: Some("x".into()),
            },
            // 全可选字段置 Some（含 last_enum）——钉板覆盖
            // skip_serializing_if 字段全现形态，键集合才完整 38 键。
            last_enum: Some(EnumSnapshot {
                at_ms: 0,
                forced: false,
                candidate_channels: 0,
                channels: Vec::new(),
                registered_slots: 0,
                error: None,
            }),
            last_enum_reason: Some("x".into()),
            channel_map_miss: 0,
            frame_tx: 0,
            frame_rx: 0,
            avg_tx_interval_ms: Some(0),
            feature_index_tx: std::collections::BTreeMap::new(),
            feature_index_rx: std::collections::BTreeMap::new(),
            arm_all_fills: 0,
            wake_successes: 0,
            wake_miss_no_path: 0,
            wake_miss_zero_match: 0,
            wake_miss_no_fingerprint: 0,
            wake_miss_ambiguous: 0,
            wake_recovery_via_reenum: 0,
            probe_cap_daily_hits: 0,
            probe_backoff_skips: 0,
            guaranteed_read_attempts: 0,
            wake_success_hit_exact: 0,
            wake_success_hit_relaxed: 0,
            run_started_at: "2026-01-01T00:00:00+00:00".to_string(),
            poll_skipped_ff_cache: 0,
            poll_skipped_dead_slot: 0,
            poll_skipped_sleep: 0,
            poll_rounds_total: 0,
            poll_round_last_duration_ms: 0,
            poll_round_max_duration_ms: 0,
            slot_outcome_success: 0,
            slot_outcome_timeout: 0,
            slot_outcome_error: 0,
            slot_query_charging: 0,
            slot_query_normal: 0,
            manifest: manifest_json(1),
            exported_at: 1,
            app_version: "0.0.0-test".into(),
        };
        let v: serde_json::Value =
            serde_json::from_str(&export.to_json_string()).expect("diag export must serialize");
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort();
        let mut dict: Vec<&str> = DIAG_EXPORT_TOP_FIELDS.iter().map(|(k, _)| *k).collect();
        dict.sort();
        assert_eq!(
            keys, dict,
            "DiagExport 序列化字段集与 DIAG_EXPORT_TOP_FIELDS 字典不一致：{keys:?} vs {dict:?}"
        );
        // 字典随 manifest 输出（诊断 JSON 内嵌 manifest 即携带）。
        let m = manifest_json(1);
        let dict_out = m["fields"]["diag_export"].as_object().unwrap();
        assert_eq!(dict_out.len(), DIAG_EXPORT_TOP_FIELDS.len());
        assert_eq!(
            dict_out["wake_miss_zero_match"].as_str().unwrap(),
            DIAG_EXPORT_TOP_FIELDS
                .iter()
                .find(|(k, _)| *k == "wake_miss_zero_match")
                .unwrap()
                .1
        );
    }
}
