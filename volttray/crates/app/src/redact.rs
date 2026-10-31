//! T-02a：诊断导出文本侧脱敏（P1-6 存量补口）。
//!
//! 纯函数、仅用 std、无正则、无 Err 路径——输入任何字符串都返回
//! 一个脱敏后的字符串（导出永远成功的硬约束不允许 Err）。
//!
//! E17（报告口径 A-R11）：规则从函数内嵌实现升格为**结构化脱敏
//! 注册表**（`REDACTION_RULES`，见下）——每条规则有稳定 id / 一句话
//! 口径 / 文档锚点，可枚举、可在测试中对账，新增规则只追加不重排。
//!
//! 规则一（R1-user-path）：`C:\Users\` 后第一段路径段替换为 `<USER>`。
//! 单一形态覆盖 USERPROFILE / APPDATA / LOCALAPPDATA 三前缀（三者均含
//! `C:\Users\<名>` 主干）；用户名从路径文本解析，零环境变量依赖。
//! 输入是最终 JSON 文本，因此 JSON 转义形态（`C:\\Users\\`）与原始
//! 形态（`C:\Users\`）都识别。**G-33 起前缀匹配为 ASCII 大小写不
//! 敏感**（`c:\users\` / `C:\USERS\` 等变体同样命中），命中段保留原始
//! 大小写逐字回写、只替换用户名段。
//!
//! 规则二（R2-device-name）：设备名按收集顺序编号为「设备N」。
//! 设备名由调用方（tray 侧 export_diag）从诊断事件流 DeviceFound
//! 收集。**E17 整值替换优先**：替换执行顺序改为按名字长度降序
//! （等长保持收集顺序），长设备名先整体替换——修复原「收集顺序
//! 逐个替换」在半子串场景下的失真（如 "MX Keys" 先于 "MX Keys Mini"
//! 被收集时，长名被部分替换产生「设备1 Mini」）。编号仍按收集顺序
//! （「设备N」的 N 与收集序号一致，不随替换执行顺序变化），因此
//! 既有导出里同一设备跨字段的编号对应关系不变；改变的只是互为
//! 子串场景下不再出现半替换残留。脱敏安全性两序等价（所有设备名
//! 均被覆盖），整值优先只影响可读性。**G-33 起设备名匹配同为
//! ASCII 大小写不敏感**（如收集名 "MX Audit Pro" 与日志文本
//! "mx audit pro" 互相命中）。

/// 脱敏规则注册表条目（E17）。字段全部为静态文本，注册表是
/// 编译期常量——审计与测试可枚举全部规则而无需执行脱敏。
pub struct RedactionRule {
    /// 稳定规则 id（测试钉板；新增规则只追加、不重排、不改 id）。
    pub id: &'static str,
    /// 一句话口径（与模块文档对应段落一致）。
    pub summary: &'static str,
    /// 文档锚点：docs/experience/05-diag-export-field-audit.md 章节。
    pub doc_anchor: &'static str,
}

/// 结构化脱敏注册表（E17，A-R11）。当前两条全文级规则；
/// 设备名编号语义（收集顺序 → 设备N）属 R2 的口径一部分。
pub const REDACTION_RULES: &[RedactionRule] = &[
    RedactionRule {
        id: "R1-user-path",
        summary: "C:\\Users\\<段>（含 JSON 转义形态，ASCII 大小写不敏感）的 <段> → <USER>",
        doc_anchor: "05 文档第一节规则一",
    },
    RedactionRule {
        id: "R2-device-name",
        summary: "设备名按收集顺序 → 设备N；整值替换优先（长名先替换）；ASCII 大小写不敏感匹配",
        doc_anchor: "05 文档第一节规则二",
    },
];

/// 对文本执行注册表全部规则（当前：规则一＋规则二）的完整脱敏。
///
/// - `C:\Users\<段>`（含 JSON 转义形态 `C:\\Users\\<段>`，ASCII 大小写
///   不敏感，G-33）的 `<段>` 替换为 `<USER>`；
/// - `device_names` 中收集序第 i 个名字（ASCII 大小写不敏感，G-33）
///   替换为「设备i+1」；替换执行按名字长度降序（E17 整值替换优先），
///   等长保持收集顺序；
/// - 其余字符逐字保留；空设备名跳过（避免空串替换把全文打碎）。
pub fn redact_text(input: &str, device_names: &[String]) -> String {
    let mut out = redact_user_paths(input);
    // E17 整值替换优先：按名字长度降序执行替换（等长稳定保持收集
    // 顺序），编号仍取原收集序号——长名先整体替换，避免短名先把
    // 长名的前缀部分吃掉产生半替换失真。
    let mut order: Vec<(usize, &String)> = device_names.iter().enumerate().collect();
    order.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(&b.0)));
    for (idx, name) in order {
        if name.is_empty() {
            continue;
        }
        let repl = format!("设备{}", idx + 1);
        // G-33：ASCII 大小写不敏感命中——导出文本里设备名的大小写
        // 变体（日志模板与收集名不一致时）同样必须脱敏。
        if find_ascii_ci(&out, name).is_some() {
            out = replace_ascii_ci(&out, name, &repl);
        }
    }
    out
}

/// G-33：ASCII 大小写不敏感子串查找，返回首个命中处的字节下标。
///
/// 逐字节比较：ASCII 字母按 `eq_ignore_ascii_case` 折叠，非 ASCII
/// 字节要求完全相等。返回值必落在 char 边界上——needle 非空且双方
/// 均为合法 UTF-8 时，首字节匹配不可能落在多字节序列内部（ASCII
/// 字节与多字节续字节 0x80–0xBF 互不折叠，非 ASCII 首字节亦然），
/// 末字节同理，故切片安全。
fn find_ascii_ci(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() || needle.len() > haystack.len() {
        return None;
    }
    let h = haystack.as_bytes();
    let n = needle.as_bytes();
    (0..=haystack.len() - needle.len()).find(|&i| {
        h[i..i + n.len()]
            .iter()
            .zip(n.iter())
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
    })
}

/// G-33：ASCII 大小写不敏感的全量替换。命中段（无论原始大小写）
/// 被 `to` 整体替换；非命中字符逐字保留。`from` 为空时恒等返回。
fn replace_ascii_ci(input: &str, from: &str, to: &str) -> String {
    if from.is_empty() {
        return input.to_string();
    }
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(i) = find_ascii_ci(rest, from) {
        out.push_str(&rest[..i]);
        out.push_str(to);
        rest = &rest[i + from.len()..];
    }
    out.push_str(rest);
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

/// 以 `prefix` 定位（G-33 起 ASCII 大小写不敏感），替换其后直到下一道
/// 反斜杠或引号之间的非空段为 `<USER>`。段为空（前缀后直接是分隔符）
/// 视为无用户名，原样保留。命中前缀保留原文大小写逐字回写——只改
/// 用户名段，不归一化路径前缀形态。
fn redact_with_prefix(input: &str, prefix: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    loop {
        match find_ascii_ci(rest, prefix) {
            None => {
                out.push_str(rest);
                return out;
            }
            Some(i) => {
                out.push_str(&rest[..i]);
                out.push_str(&rest[i..i + prefix.len()]);
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

    /// E17 整值替换优先：互为子串的设备名（短名先收集）不再产生
    /// 半替换失真——长名先整体替换；编号仍按收集顺序。
    #[test]
    fn whole_value_priority_replaces_longest_first() {
        let names = vec!["MX Keys".to_string(), "MX Keys Mini".to_string()];
        let input = "{\"devices\":[{\"name\":\"MX Keys Mini\"},{\"name\":\"MX Keys\"}]}";
        let out = redact_text(input, &names);
        assert!(
            out.contains(r#"{"name":"设备2"},{"name":"设备1"}"#),
            "长名整体替换为 设备2、短名为 设备1，无半替换残留：{out}"
        );
        assert!(!out.contains("Mini"), "不得残留长名尾部：{out}");
        assert!(!out.contains("MX Keys"), "不得残留设备名：{out}");
    }

    /// E17 注册表钉板：规则集合（id 序列）与文档锚点锁定——新增
    /// 规则只追加；删改既有 id 在此失败，强制同步 05 文档与审计。
    #[test]
    fn redaction_rules_registry_is_pinned() {
        let ids: Vec<&str> = REDACTION_RULES.iter().map(|r| r.id).collect();
        assert_eq!(ids, vec!["R1-user-path", "R2-device-name"]);
        for rule in REDACTION_RULES {
            assert!(!rule.summary.is_empty());
            assert!(rule.doc_anchor.contains("05 文档"));
        }
    }

    /// E17 golden：含令牌值（用户名路径 + 设备名）的错误帧全文比对。
    /// 帧形态取自 05 文档必查点②（hid open 错误原文可能含路径）——
    /// 固定输入 → 固定输出逐字相等，任何规则行为漂移都会打破 golden。
    #[test]
    fn golden_error_frame_with_token_values() {
        let input = concat!(
            r#"{"seq":7,"layer":"transport","type":"io_error","#,
            r#""message":"hid open failed: C:\Users\audit_tok\AppData\Roaming\LogiVoltTray\x.log (MX Audit Pro)"}"#
        );
        let out = redact_text(input, &["MX Audit Pro".to_string()]);
        assert_eq!(
            out,
            concat!(
                r#"{"seq":7,"layer":"transport","type":"io_error","#,
                r#""message":"hid open failed: C:\Users\<USER>\AppData\Roaming\LogiVoltTray\x.log (设备1)"}"#
            )
        );
    }

    /// G-33 规则一：原始形态路径前缀的大小写变体（小写盘符 / 全大写
    /// USERS / 混合）同样脱敏；命中前缀保留原文大小写，只换用户名段。
    #[test]
    fn redacts_username_case_variants_in_raw_paths() {
        for path in [
            r"c:\users\zhangsan\AppData\x.log",
            r"C:\USERS\zhangsan\AppData\x.log",
            r"c:\Users\zhangsan\AppData\x.log",
            r"C:\users\zhangsan\AppData\x.log",
        ] {
            let out = redact_text(path, &[]);
            assert!(
                !out.contains("zhangsan"),
                "大小写变体不得残留用户名：{path} -> {out}"
            );
            assert!(out.contains("<USER>"), "变体必须替换为 <USER>：{out}");
            assert!(
                out.ends_with(r"\AppData\x.log"),
                "用户名段之后逐字保留：{out}"
            );
        }
        // 前缀原文大小写保留（不归一化）：只改用户名段。
        let out = redact_text(r"c:\USERS\zhangsan\x", &[]);
        assert_eq!(out, r"c:\USERS\<USER>\x");
    }

    /// G-33 规则一：JSON 转义形态（双反斜杠）的大小写变体同样脱敏。
    #[test]
    fn redacts_username_case_variants_in_json_escaped_paths() {
        let input = r#"{"path":"C:\\USERS\\zhangsan\\AppData\\x.log"}"#;
        let out = redact_text(input, &[]);
        assert!(!out.contains("zhangsan"), "转义变体不得残留用户名：{out}");
        assert!(
            out.contains(r#"C:\\USERS\\<USER>\\"#),
            "转义形态前缀原文保留、用户名段替换：{out}"
        );
    }

    /// G-33 规则二：设备名大小写变体（日志文本与收集名大小写不一致）
    /// 同样替换为「设备N」；其余字符逐字保留。
    #[test]
    fn redacts_device_names_case_variants() {
        let names = vec!["MX Audit Pro".to_string()];
        let input = "open failed for mx audit pro (slot #2); MX AUDIT PRO retry";
        let out = redact_text(input, &names);
        assert_eq!(out, "open failed for 设备1 (slot #2); 设备1 retry");
        assert!(
            !out.to_ascii_lowercase().contains("mx audit pro"),
            "任何大小写变体都不得残留：{out}"
        );
    }

    /// G-33 golden：大小写变体错误帧全文比对（同 E17 golden 形态，
    /// 路径前缀与设备名均取大小写变体）——固定输入 → 固定输出逐字
    /// 相等，大小写不敏感匹配失效（退化为精确匹配）即打破 golden。
    #[test]
    fn golden_error_frame_case_variants() {
        let input = concat!(
            r#"{"seq":8,"layer":"transport","type":"io_error","#,
            r#""message":"hid open failed: c:\USERS\audit_tok\AppData\Roaming\LogiVoltTray\x.log (mx AUDIT pro)"}"#
        );
        let out = redact_text(input, &["MX Audit Pro".to_string()]);
        assert_eq!(
            out,
            concat!(
                r#"{"seq":8,"layer":"transport","type":"io_error","#,
                r#""message":"hid open failed: c:\USERS\<USER>\AppData\Roaming\LogiVoltTray\x.log (设备1)"}"#
            )
        );
    }
}
