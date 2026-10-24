//! E23（报告口径 A-R4②，第 1 步）：设置窗口纯逻辑文案拼写函数从
//! tray::settings 抽离到 app crate——它们本已是纯函数（无 Win32
//! 依赖），移动后跨平台可测（原测试在 cfg(windows) 的 settings.rs
//! 内，Linux/CI 主机上跑不到）。本步只移动「文本拼写」一类
//! （tier_hint_dynamic / fine_tune_gate_text / lab_hint_text 及其
//! 常量），阈值校验等其余纯逻辑留待后续窗口逐步抽离。

use crate::config::{FONT_TIER_THRESHOLD_MAX, FONT_TIER_THRESHOLD_MIN};

/// v0.3.22（D-87/D-88）：实验室 hint 终版文案——如实说明可逐项关闭、
/// 尚未完成全量真机验证、反馈渠道与生效方式（不承诺「零事故」）。
pub const LAB_HINT_TEXT: &str = "实验室（试验功能）：可逐项关闭；尚未完成全量真机验证，如遇异常请关闭对应功能并通过 Issues 反馈。改动保存后重启生效，可点『立即重启』立即应用。";
/// 四项有未保存改动时追加的行（D-88：改动保存后需重启方完全生效）。
pub const LAB_DIRTY_TEXT: &str = "已修改，重启后生效。";

/// 分档说明示例（v0.3.6 规范偏差修正：按当前档数/阈值实时拼写，替代静态常量）。
/// 例：thresholds = [20, 50] →「阈值 1–99 严格递增；电量 ≤ 数值归入对应档（20/50 → 0–20 / 21–50 / 51–100）」。
pub fn tier_hint_dynamic(thresholds: &[u8]) -> String {
    let ths = thresholds
        .iter()
        .map(|t| t.to_string())
        .collect::<Vec<_>>()
        .join("/");
    let mut parts: Vec<String> = Vec::new();
    let mut lo: i32 = 0;
    for &t in thresholds {
        parts.push(format!("{lo}–{t}"));
        lo = t as i32 + 1;
    }
    parts.push(format!("{lo}–100"));
    format!(
        "阈值 {FONT_TIER_THRESHOLD_MIN}–{FONT_TIER_THRESHOLD_MAX} 严格递增；电量 ≤ 数值归入对应档（{ths} → {}）",
        parts.join(" / ")
    )
}

/// 闸门复选框文案（D-87）：入口行用「精细化设置」交互语义 + 常驻计数。
pub fn fine_tune_gate_text(enabled: usize) -> String {
    format!("精细化设置（试验功能，已开启 {enabled}/4 项）")
}

/// 实验室 hint 动态拼写（D-88）：四项有未保存改动时追加「已修改，重启后生效」。
pub fn lab_hint_text(dirty: bool) -> String {
    if dirty {
        format!("{LAB_HINT_TEXT}\n{LAB_DIRTY_TEXT}")
    } else {
        LAB_HINT_TEXT.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{fine_tune_gate_text, lab_hint_text, tier_hint_dynamic, LAB_HINT_TEXT};

    #[test]
    fn tier_hint_dynamic_spells_ranges() {
        assert_eq!(
            tier_hint_dynamic(&[20, 50]),
            "阈值 1–99 严格递增；电量 ≤ 数值归入对应档（20/50 → 0–20 / 21–50 / 51–100）"
        );
        assert_eq!(
            tier_hint_dynamic(&[10, 30, 50, 70]),
            "阈值 1–99 严格递增；电量 ≤ 数值归入对应档（10/30/50/70 → 0–10 / 11–30 / 31–50 / 51–70 / 71–100）"
        );
    }

    /// v0.3.22（D-87）：闸门计数常驻动态显示——N/4 实时拼写。
    #[test]
    fn fine_tune_gate_text_spells_count() {
        assert_eq!(
            fine_tune_gate_text(0),
            "精细化设置（试验功能，已开启 0/4 项）"
        );
        assert_eq!(
            fine_tune_gate_text(4),
            "精细化设置（试验功能，已开启 4/4 项）"
        );
        assert_eq!(
            fine_tune_gate_text(2),
            "精细化设置（试验功能，已开启 2/4 项）"
        );
    }

    /// v0.3.22（D-88）：hint 终版口径 + 未保存改动动态追加行。
    #[test]
    fn lab_hint_text_marks_dirty() {
        assert!(!lab_hint_text(false).contains("已修改，重启后生效"));
        assert!(lab_hint_text(true).contains("已修改，重启后生效"));
        assert!(lab_hint_text(false).contains("实验室（试验功能）"));
        assert!(LAB_HINT_TEXT.contains("尚未完成全量真机验证"));
    }
}
