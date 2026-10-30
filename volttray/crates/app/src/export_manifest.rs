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

/// 知情声明（逐字段处置留痕；纯文本，不含任何外链 token 与时区偏移
/// 值——HTML 零外链扫描与 BvA-6 双约束，均有钉板）。
pub const MANIFEST_NOTES: &[&str] = &[
    "run_start/run_stop 秒级时间线保留（U-2C 裁决原文：精度不动）",
    "HTML 首末时刻字段保留（G-34 已登记；剥离/降精度挂 U-2 拍板，本批不落地）",
    "display_name 型号默认保留（U-2C）；anonymize=on 时按导出会话映射为设备N（C-3：会话内稳定、不承诺跨导出稳定）",
    "activity_buckets 逐分钟活动位图默认不导出（组4 N-08② export_activity_bitmap 默认关，与 anonymize 解耦；勾选后随事件流并入）",
    "manifest 只写规则名，不含采集时区偏移值（BvA-6）",
];

/// 生成 manifest JSON 值。`generated_at` 为导出时刻 epoch 秒（调用侧
/// 注入，不在此取系统时间——保持纯函数可钉板）。
pub fn manifest_json(generated_at: i64) -> serde_json::Value {
    let rules: Vec<serde_json::Value> = REDACTION_RULES
        .iter()
        .map(|r| serde_json::json!({ "id": r.id, "summary": r.summary }))
        .collect();
    serde_json::json!({
        "schema_version": MANIFEST_SCHEMA_VERSION,
        "app_version": env!("CARGO_PKG_VERSION"),
        "generated_at": generated_at,
        "fields": {
            "stats_json": STATS_JSON_TOP_FIELDS,
            "csv_columns": CSV_COLUMNS,
            "html_data": HTML_DATA_TOP_FIELDS,
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
}
