//! 设备名消毒（评审 P1-5）。
//!
//! 设备名来自硬件字符串，进入 UI 文本、tooltip、诊断导出前必须消毒：
//! 1. 空白字符（含 tab/换行）折叠为单个空格；其余 C0/C1 控制字符与 DEL 丢弃；
//! 2. 白名单外字符替换为 `?`（白名单：字母数字 + 常用符号）；
//! 3. 去首尾空白，折叠内部连续空白；
//! 4. 截断到 64 字符；
//! 5. 结果为空 → "未知设备"。

pub const MAX_DEVICE_NAME_CHARS: usize = 64;
pub const FALLBACK_DEVICE_NAME: &str = "未知设备";

fn is_allowed(c: char) -> bool {
    c.is_alphanumeric()
        || matches!(
            c,
            ' ' | '-' | '_' | '+' | '.' | '(' | ')' | '[' | ']' | '/' | '·'
        )
}

pub fn device_name(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().min(MAX_DEVICE_NAME_CHARS));
    let mut pending_space = false;
    for c in raw.chars() {
        if c.is_whitespace() {
            if !out.is_empty() {
                pending_space = true; // 任意空白（含 tab/换行）折叠为单个空格、丢弃前导空白
            }
            continue;
        }
        if c.is_control() {
            continue; // 其余 C0/C1/DEL 直接丢弃
        }
        let c = if is_allowed(c) { c } else { '?' };
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(c);
        if out.chars().count() >= MAX_DEVICE_NAME_CHARS {
            break;
        }
    }
    let trimmed = out.trim_end().to_string();
    if trimmed.is_empty() {
        FALLBACK_DEVICE_NAME.to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_name_passthrough() {
        assert_eq!(device_name("MX Master 3S"), "MX Master 3S");
    }

    #[test]
    fn control_chars_stripped() {
        assert_eq!(device_name("MX\u{0007}Master\u{009F}3"), "MXMaster3");
        assert_eq!(device_name("A\u{0000}B\u{007F}C"), "ABC");
    }

    #[test]
    fn whitelist_replacement() {
        assert_eq!(device_name("Mouse<Pro>"), "Mouse?Pro?");
        assert_eq!(device_name("K860\tKeyboard"), "K860 Keyboard");
    }

    #[test]
    fn middle_dot_separator_kept() {
        // v0.3.0 根因回归：槽位名分隔符 U+00B7（·）曾被消毒白名单替换成「?」。
        assert_eq!(device_name("USB Receiver · 槽位 1"), "USB Receiver · 槽位 1");
        assert_eq!(device_name("MX ·2"), "MX ·2");
    }

    #[test]
    fn cjk_kept() {
        assert_eq!(device_name("罗技鼠标"), "罗技鼠标");
    }

    #[test]
    fn length_capped_at_64() {
        let long = "X".repeat(200);
        assert_eq!(device_name(&long).chars().count(), 64);
    }

    #[test]
    fn empty_or_all_control_falls_back() {
        assert_eq!(device_name(""), FALLBACK_DEVICE_NAME);
        assert_eq!(
            device_name("\u{0001}\u{0002}\u{0003}"),
            FALLBACK_DEVICE_NAME
        );
        assert_eq!(device_name("   "), FALLBACK_DEVICE_NAME);
    }

    #[test]
    fn whitespace_collapsed() {
        assert_eq!(device_name("MX   Master    3S"), "MX Master 3S");
    }
}
