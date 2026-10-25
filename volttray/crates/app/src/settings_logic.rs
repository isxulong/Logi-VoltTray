//! E23（报告口径 A-R4②，第 2 步）：设置窗口「阈值校验 / 档位映射」
//! 纯逻辑从 tray::settings 抽离到 app crate——抽离前它们内嵌在
//! on_command 的控件分支里（cfg(windows)，Linux/CI 主机跑不到测试），
//! 抽离后为自由函数、跨平台受测。本步为纯移动 + 行为钉板测试，
//! 行为零变化（逐行对照原分支语义）。
//!
//! 抽离清单（原位置：tray/src/settings.rs on_command）：
//! - 阈值数字框校验：`parse_tier_threshold`（1–99 整数解析）＋
//!   `thresholds_strictly_increasing`（严格递增校验）＋两处错误文案常量；
//! - 档数调整映射：`retier_thresholds`（目标档数 → 阈值数组重排，
//!   增长时在 [上一档+1, 99] 均分剩余档位）。

use crate::config::FONT_TIER_THRESHOLD_MAX;
use crate::config::FONT_TIER_THRESHOLD_MIN;

/// 阈值数字框校验错误文案：非 1–99 整数（原文案逐字保留，F2 口径 D-60）。
pub const TIER_THRESHOLD_ERR_NOT_INT_IN_RANGE: &str = "⚠ 需为 1–99 整数";
/// 阈值数字框校验错误文案：非严格递增（原文案逐字保留）。
pub const TIER_THRESHOLD_ERR_NOT_INCREASING: &str = "⚠ 需严格递增";

/// 阈值数字框文本 → 阈值：1–99 整数（`i64` 解析后区间判断，与原
/// `text.parse::<i64>()` 语义逐字一致——前导 `+` 可解析、空白不可）。
/// 空字符串为输入中间态：调用方在空调用前先行拦截，本函数对空串
/// 仍返回 Err（解析失败），不改变调用方语义。
pub fn parse_tier_threshold(text: &str) -> Result<u8, &'static str> {
    match text.parse::<i64>() {
        Ok(v) if (FONT_TIER_THRESHOLD_MIN as i64..=FONT_TIER_THRESHOLD_MAX as i64).contains(&v) => {
            Ok(v as u8)
        }
        _ => Err(TIER_THRESHOLD_ERR_NOT_INT_IN_RANGE),
    }
}

/// 严格递增校验：`windows(2).all(w[0] < w[1])`——空数组与单元素数组
/// 按原语义判定为真（无相邻对可违反）。
pub fn thresholds_strictly_increasing(ths: &[u8]) -> bool {
    ths.windows(2).all(|w| w[0] < w[1])
}

/// 档数调整映射：把当前阈值数组重排到目标档数 `n`（阈值数 = n - 1）。
/// 收缩直接弹尾；增长时在 [上一档+1, MAX] 区间均分剩余档位
/// （原 on_command IDC_TIER_COUNT 分支逐行平移；返回值由调用方再过
/// `normalize_font_tiers` 兜底，与原分支一致）。无严格递增空间时
/// 提前停止（break 语义保留），余下交给 normalize 修复。
///
/// 修复（小修批第 1 项，2026-10-25 批次）：旧实现末档阈值已为 MAX(99)
/// 时继续增档，`next.clamp(lo + 1, MAX)` 因 min(100) > max(99) 直接
/// panic（clamp 先于 break 触发，break 实为不可达安全阀；v0.3.23 曾以
/// `#[should_panic]` 钉板该缺陷）。现在进入 clamp 前先判「无严格递增
/// 空间」（lo >= MAX）直接 break——panic 路径消除，余下仍交 normalize
/// 兜底，钉板测试改为断言「不 panic + 原样返回」。`slots == 0` 的
/// else 分支在原代码中即不可达（while 条件保证 slots ≥ 1），逐字保留。
pub fn retier_thresholds(ths: &[u8], n: usize) -> Vec<u8> {
    let max = FONT_TIER_THRESHOLD_MAX as i32;
    let mut ths = ths.to_vec();
    while ths.len() + 1 > n {
        ths.pop();
    }
    while ths.len() + 1 < n {
        let lo = ths.last().map(|t| *t as i32).unwrap_or(0);
        let slots = n - ths.len() - 1;
        // 小修批第 1 项：末档已达 MAX 时无严格递增空间，直接停止——
        // 否则下方 clamp(lo + 1, MAX) 因 min > max panic。
        if lo >= max {
            break;
        }
        let next = if slots > 0 {
            (max - lo) / (slots as i32 + 1) + lo
        } else {
            (max + lo) / 2
        };
        let next = next.clamp(lo + 1, max) as u8;
        if next as i32 <= lo {
            break; // 无严格递增空间；normalize 会兜底。
        }
        ths.push(next);
    }
    ths
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 行为钉板：阈值解析的边界与原分支逐字一致（1–99 闭区间、
    /// i64 解析口径——前导 `+` 接受、空白与小数拒绝）。
    #[test]
    fn parse_tier_threshold_pins_range_and_parse_semantics() {
        assert_eq!(parse_tier_threshold("1"), Ok(1));
        assert_eq!(parse_tier_threshold("20"), Ok(20));
        assert_eq!(parse_tier_threshold("99"), Ok(99));
        assert_eq!(parse_tier_threshold("+30"), Ok(30)); // i64 解析接受前导 +
        assert_eq!(
            parse_tier_threshold("0"),
            Err(TIER_THRESHOLD_ERR_NOT_INT_IN_RANGE)
        );
        assert_eq!(
            parse_tier_threshold("100"),
            Err(TIER_THRESHOLD_ERR_NOT_INT_IN_RANGE)
        );
        assert_eq!(
            parse_tier_threshold("-5"),
            Err(TIER_THRESHOLD_ERR_NOT_INT_IN_RANGE)
        );
        assert_eq!(
            parse_tier_threshold("abc"),
            Err(TIER_THRESHOLD_ERR_NOT_INT_IN_RANGE)
        );
        assert_eq!(
            parse_tier_threshold(""),
            Err(TIER_THRESHOLD_ERR_NOT_INT_IN_RANGE)
        );
        assert_eq!(
            parse_tier_threshold(" 30"),
            Err(TIER_THRESHOLD_ERR_NOT_INT_IN_RANGE)
        ); // 空白不 trim
        assert_eq!(
            parse_tier_threshold("30.0"),
            Err(TIER_THRESHOLD_ERR_NOT_INT_IN_RANGE)
        );
    }

    /// 行为钉板：严格递增校验（空/单元素为真，与 windows(2) 语义一致）。
    #[test]
    fn thresholds_strictly_increasing_pins_windows_semantics() {
        assert!(thresholds_strictly_increasing(&[20, 50]));
        assert!(thresholds_strictly_increasing(&[]));
        assert!(thresholds_strictly_increasing(&[7]));
        assert!(!thresholds_strictly_increasing(&[50, 20]));
        assert!(!thresholds_strictly_increasing(&[20, 20]));
        assert!(!thresholds_strictly_increasing(&[10, 30, 30, 70]));
    }

    /// 行为钉板：档数调整映射的均分输出与原分支逐步手算一致
    /// （2026-10-24 手工对账：99 上限、+1 下限、整数除法截断）。
    #[test]
    fn retier_thresholds_pins_even_split_outputs() {
        // 收缩：弹尾
        assert_eq!(retier_thresholds(&[20, 50], 2), vec![20]);
        assert_eq!(retier_thresholds(&[10, 30, 50, 70], 3), vec![10, 30]);
        // 增长：均分剩余档位
        assert_eq!(retier_thresholds(&[20, 50], 4), vec![20, 50, 74]);
        assert_eq!(retier_thresholds(&[], 3), vec![33, 66]);
        assert_eq!(retier_thresholds(&[], 5), vec![19, 39, 59, 79]);
        // 原样：目标档数等于当前
        assert_eq!(retier_thresholds(&[20, 50], 3), vec![20, 50]);
        // 末档接近上限但仍有递增空间：slots=1 → (99-97)/2+97 = 98
        assert_eq!(retier_thresholds(&[20, 97], 4), vec![20, 97, 98]);
    }

    /// 修复钉板（小修批第 1 项）：末档阈值已为 99 时继续增档不再
    /// panic——无严格递增空间提前 break、原样返回，余下交 normalize
    /// 兜底（v0.3.23 曾以 #[should_panic] 钉住旧缺陷，本批修复后改
    /// 为正向断言）。
    #[test]
    fn retier_thresholds_trailing_99_stops_without_panic() {
        assert_eq!(retier_thresholds(&[98, 99], 4), vec![98, 99]);
        assert_eq!(retier_thresholds(&[99], 3), vec![99]);
        // 全 99 多档目标同样不再 panic。
        assert_eq!(retier_thresholds(&[99], 5), vec![99]);
    }
}
