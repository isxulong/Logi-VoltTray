//! T-02a：诊断导出文本侧脱敏（P1-6 存量补口）。
//!
//! 纯函数、仅用 std、无正则、无 Err 路径——输入任何字符串都返回
//! 一个脱敏后的字符串（导出永远成功的硬约束不允许 Err）。
//!
//! 规则一：`C:\Users\` 后第一段路径段替换为 `<USER>`。单一形态覆盖
//! USERPROFILE / APPDATA / LOCALAPPDATA 三前缀（三者均含 `C:\Users\<名>`
//! 主干）；用户名从路径文本解析，零环境变量依赖。输入是最终 JSON 文本，
//! 因此 JSON 转义形态（`C:\\Users\\`）与原始形态（`C:\Users\`）都识别。
//!
//! 规则二：设备名按出现顺序替换为「设备N」（N 从 1 起，按收集顺序）。
//! 设备名由调用方（tray 侧 export_diag）从诊断事件流 DeviceFound 收集。

/// 对文本执行规则一＋规则二的完整脱敏。
///
/// - `C:\Users\<段>`（含 JSON 转义形态 `C:\\Users\\<段>`）的 `<段>`
///   替换为 `<USER>`；
/// - `device_names` 中第 i 个名字（收集顺序即出现顺序）替换为「设备i+1」；
/// - 其余字符逐字保留；空设备名跳过（避免空串替换把全文打碎）。
pub fn redact_text(input: &str, device_names: &[String]) -> String {
    let mut out = redact_user_paths(input);
    for (idx, name) in device_names.iter().enumerate() {
        if name.is_empty() {
            continue;
        }
        let repl = format!("设备{}", idx + 1);
        if out.contains(name.as_str()) {
            out = out.replace(name.as_str(), &repl);
        }
    }
    out
}

/// 规则一：`C:\Users\` 后第一段路径段 → `<USER>`。
/// 先处理 JSON 转义形态（`C:\\Users\\`），再处理原始形态（`C:\Users\`）。
fn redact_user_paths(input: &str) -> String {
    // JSON 转义形态：文本中字面为 C:\\Users\\（每处两道反斜杠）。
    let escaped = redact_with_prefix(input, "C:\\\\Users\\\\");
    // 原始形态：文本中字面为 C:\Users\（单道反斜杠）。
    redact_with_prefix(&escaped, "C:\\Users\\")
}

/// 以 `prefix` 定位，替换其后直到下一道反斜杠或引号之间的非空段为 `<USER>`。
/// 段为空（前缀后直接是分隔符）视为无用户名，原样保留。
fn redact_with_prefix(input: &str, prefix: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    loop {
        match rest.find(prefix) {
            None => {
                out.push_str(rest);
                return out;
            }
            Some(i) => {
                out.push_str(&rest[..i]);
                out.push_str(prefix);
                let after = &rest[i + prefix.len()..];
                // 段终止：下一道反斜杠（路径分隔）或引号（字符串结束）。
                let seg_end = after
                    .char_indices()
                    .find(|(_, c)| *c == '\\' || *c == '"')
                    .map(|(j, _)| j)
                    .unwrap_or(after.len());
                let seg = &after[..seg_end];
                if seg.is_empty() {
                    // 无用户名形态（如 C:\Users\ 直接连分隔符）：不替换。
                    out.push_str(&after[..seg_end]);
                } else {
                    out.push_str("<USER>");
                }
                rest = &after[seg_end..];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 规则一（JSON 转义形态，诊断导出的真实形态）：固定假用户名，
    /// 断言含 `<USER>`、不含原名、其余字段逐字保留。不读环境变量。
    #[test]
    fn redacts_username_in_json_escaped_paths() {
        let input = r#"{"log_tail":{"path":"C:\\Users\\zhangsan\\AppData\\Roaming\\LogiVoltTray\\logi-volttray.log","lines":["ok"]},"app_version":"0.3.19"}"#;
        let out = redact_text(input, &[]);
        assert!(
            out.contains(r#"C:\\Users\\<USER>\\"#),
            "用户名段必须替换为 <USER>"
        );
        assert!(!out.contains("zhangsan"), "不得残留原用户名");
        assert!(
            out.contains(r#""app_version":"0.3.19""#),
            "其余字段逐字保留"
        );
    }

    /// 规则一（原始单反斜杠形态）。
    #[test]
    fn redacts_username_in_raw_paths() {
        let input = r"open error at C:\Users\zhangsan\AppData\Local\Temp\x.log";
        let out = redact_text(input, &[]);
        assert!(out.contains(r"C:\Users\<USER>\"), "原始形态同样脱敏");
        assert!(!out.contains("zhangsan"));
    }

    /// 规则二：设备名按收集顺序替换为「设备N」，其余内容逐字保留。
    #[test]
    fn redacts_device_names_in_order() {
        let names = vec![
            "MX Master 3S Wireless Mouse".to_string(),
            "MX Keys Mini".to_string(),
        ];
        let input = "{\"devices\":[{\"name\":\"MX Keys Mini\"},{\"name\":\"MX Master 3S Wireless Mouse\"}]}";
        let out = redact_text(input, &names);
        assert!(out.contains("设备1"), "第一个设备名 → 设备1");
        assert!(out.contains("设备2"), "第二个设备名 → 设备2");
        assert!(!out.contains("MX Master"), "不得残留设备名");
        assert!(!out.contains("MX Keys"), "不得残留设备名");
        assert!(
            out.contains("\"devices\":[{\"name\":\""),
            "JSON 结构其余字符保留"
        );
    }

    /// 无匹配时不改一个字符（恒等）；空设备名不产生替换。
    #[test]
    fn no_match_is_identity_and_empty_names_skipped() {
        let input = "plain text without paths or names";
        let out = redact_text(input, &[String::new(), "不存在的设备".to_string()]);
        assert_eq!(out, input, "无命中时逐字恒等");
    }
}
