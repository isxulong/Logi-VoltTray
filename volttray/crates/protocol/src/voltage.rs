//! 电压→电量百分比映射（fixtures/voltage_curves.json，分段线性插值）。
//!
//! 曲线锚点来自 PRD 协议附录：
//! - Li-ion：4.2V→100%，3.5V 平台区，3.3V→0%
//! - AAA 碱性：1.6V→100%，1.1V→0%

use std::sync::OnceLock;

const CURVES_JSON: &str = include_str!("../../../fixtures/voltage_curves.json");

/// 一条电压曲线：锚点按毫伏降序排列。
#[derive(Debug, Clone, PartialEq)]
pub struct Curve {
    pub name: String,
    /// (毫伏, 百分比)，按毫伏降序。
    pub points: Vec<(u16, u8)>,
}

#[derive(Debug, serde::Deserialize)]
struct CurvesFile {
    curves: Vec<CurveRaw>,
}

#[derive(Debug, serde::Deserialize)]
struct CurveRaw {
    name: String,
    points: Vec<(u16, u8)>,
}

#[derive(Debug)]
pub struct CurveTable {
    curves: Vec<Curve>,
}

static TABLE: OnceLock<CurveTable> = OnceLock::new();

fn table() -> &'static CurveTable {
    TABLE.get_or_init(|| {
        let file: CurvesFile = serde_json::from_str(CURVES_JSON)
            .expect("fixtures/voltage_curves.json must be valid JSON");
        let mut curves = Vec::with_capacity(file.curves.len());
        for raw in file.curves {
            let mut points = raw.points;
            points.sort_by(|a, b| b.0.cmp(&a.0));
            curves.push(Curve {
                name: raw.name,
                points,
            });
        }
        CurveTable { curves }
    })
}

impl Curve {
    /// 分段线性插值；超出锚点范围按端点钳制。
    pub fn percent_at(&self, mv: u16) -> u8 {
        let Some(first) = self.points.first() else {
            return 0;
        };
        if mv >= first.0 {
            return first.1;
        }
        let Some(last) = self.points.last() else {
            return 0;
        };
        if mv <= last.0 {
            return last.1;
        }
        for pair in self.points.windows(2) {
            let (hi_mv, hi_pct) = pair[0];
            let (lo_mv, lo_pct) = pair[1];
            if mv <= hi_mv && mv >= lo_mv {
                let span_mv = (hi_mv - lo_mv) as i32;
                if span_mv == 0 {
                    return hi_pct;
                }
                let span_pct = hi_pct as i32 - lo_pct as i32;
                let off = (mv - lo_mv) as i32;
                // 四舍五入（v0.2.0：真机 13 锚点曲线 4019mV → 84% 而非 83%，
                // 参考实现 round() 实证）。半进位：±span_mv 再除 2×span_mv。
                let pct = lo_pct as i32 + (span_pct * off * 2 + span_mv) / (span_mv * 2);
                return pct.clamp(0, 100) as u8;
            }
        }
        0
    }
}

/// 按曲线名取百分比；曲线不存在返回 None。
pub fn percent_from_mv(curve: &str, mv: u16) -> Option<u8> {
    table()
        .curves
        .iter()
        .find(|c| c.name == curve)
        .map(|c| c.percent_at(mv))
}

pub fn curve_names() -> Vec<&'static str> {
    table().curves.iter().map(|c| c.name.as_str()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn liion_endpoints_clamp() {
        assert_eq!(percent_from_mv("li-ion", 4200), Some(100));
        assert_eq!(percent_from_mv("li-ion", 9999), Some(100));
        assert_eq!(percent_from_mv("li-ion", 3300), Some(0));
        assert_eq!(percent_from_mv("li-ion", 1000), Some(0));
    }

    #[test]
    fn liion_midpoint_interpolates() {
        // 3500→10, 3700→25：3600 应落在 17/18 之间。
        let p = percent_from_mv("li-ion", 3600).expect("curve");
        assert!((15..=20).contains(&p), "got {p}");
    }

    #[test]
    fn alkaline_curve_exists() {
        assert_eq!(percent_from_mv("alkaline-aaa", 1600), Some(100));
        assert_eq!(percent_from_mv("alkaline-aaa", 1100), Some(0));
    }

    #[test]
    fn unknown_curve_is_none() {
        assert_eq!(percent_from_mv("nope", 3700), None);
    }

    #[test]
    fn liion_logi_matches_reference_anchors() {
        // 参考实现真机 13 锚点曲线（fixtures/battery_map_cases.json）。
        assert_eq!(percent_from_mv("li-ion-logi", 4186), Some(100));
        assert_eq!(percent_from_mv("li-ion-logi", 5000), Some(100));
        assert_eq!(percent_from_mv("li-ion-logi", 4019), Some(84));
        assert_eq!(percent_from_mv("li-ion-logi", 3989), Some(80));
        assert_eq!(percent_from_mv("li-ion-logi", 3811), Some(50));
        assert_eq!(percent_from_mv("li-ion-logi", 3646), Some(5));
        assert_eq!(percent_from_mv("li-ion-logi", 3579), Some(2));
        assert_eq!(percent_from_mv("li-ion-logi", 3500), Some(0));
        assert_eq!(percent_from_mv("li-ion-logi", 3000), Some(0));
    }

    #[test]
    fn liion_logi_monotonic_non_increasing_mv() {
        // 毫伏升 → 百分比不降（红队：曲线必须单调）。
        let mut last = 0u8;
        for mv in (3500..=4200).step_by(7) {
            let p = percent_from_mv("li-ion-logi", mv).expect("curve");
            assert!(p >= last, "not monotonic at {mv}mV: {p} < {last}");
            last = p;
        }
    }
}
