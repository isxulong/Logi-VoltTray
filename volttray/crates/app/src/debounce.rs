//! 双阈值去抖（PRD M3 + 评审 P1-3 数值口径）。
//!
//! 规则：连续 2 次读数低于阈值才触发一次告警；读数回升到阈值之上
//! （或设备进入充电）即复位，之后可再次触发。critical 优先于 warn。

/// 告警类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alert {
    /// 低于 warn 阈值（含百分比）。
    Warn(u8),
    /// 低于 critical 阈值。
    Critical(u8),
}

/// 触发所需的连续越限次数（评审 P1-3）。
pub const DEBOUNCE_COUNT: u8 = 2;

#[derive(Debug, Clone)]
pub struct ThresholdDebouncer {
    warn_below: u8,
    critical_below: u8,
    warn_streak: u8,
    critical_streak: u8,
    warn_fired: bool,
    critical_fired: bool,
}

impl ThresholdDebouncer {
    pub fn new(warn_below: u8, critical_below: u8) -> Self {
        debug_assert!(critical_below < warn_below);
        ThresholdDebouncer {
            warn_below,
            critical_below,
            warn_streak: 0,
            critical_streak: 0,
            warn_fired: false,
            critical_fired: false,
        }
    }

    /// 喂入一次读数；返回本次应发的告警（若有）。
    pub fn update(&mut self, percent: u8, charging: bool) -> Option<Alert> {
        if charging {
            self.reset();
            return None;
        }

        if percent < self.critical_below {
            self.critical_streak = self.critical_streak.saturating_add(1);
        } else {
            self.critical_streak = 0;
            self.critical_fired = false;
        }
        if percent < self.warn_below {
            self.warn_streak = self.warn_streak.saturating_add(1);
        } else {
            self.warn_streak = 0;
            self.warn_fired = false;
        }

        if self.critical_streak >= DEBOUNCE_COUNT && !self.critical_fired {
            self.critical_fired = true;
            // critical 触发时抑制同周期的 warn 重复提醒。
            self.warn_fired = true;
            return Some(Alert::Critical(percent));
        }
        if self.warn_streak >= DEBOUNCE_COUNT && !self.warn_fired {
            self.warn_fired = true;
            return Some(Alert::Warn(percent));
        }
        None
    }

    pub fn reset(&mut self) {
        self.warn_streak = 0;
        self.critical_streak = 0;
        self.warn_fired = false;
        self.critical_fired = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_consecutive_below_warn_fires_once() {
        let mut d = ThresholdDebouncer::new(20, 10);
        assert_eq!(d.update(19, false), None); // 第 1 次：不触发
        assert_eq!(d.update(18, false), Some(Alert::Warn(18))); // 第 2 次：触发
        assert_eq!(d.update(17, false), None); // 已触发：不重复
    }

    #[test]
    fn recovery_resets_and_can_fire_again() {
        let mut d = ThresholdDebouncer::new(20, 10);
        d.update(19, false);
        assert_eq!(d.update(18, false), Some(Alert::Warn(18)));
        d.update(50, false); // 回升：复位
        assert_eq!(d.update(19, false), None);
        assert_eq!(d.update(18, false), Some(Alert::Warn(18)));
    }

    #[test]
    fn charging_resets() {
        let mut d = ThresholdDebouncer::new(20, 10);
        d.update(15, false);
        d.update(15, true); // 充电：复位
        assert_eq!(d.update(15, false), None);
    }

    #[test]
    fn critical_dominates() {
        let mut d = ThresholdDebouncer::new(20, 10);
        assert_eq!(d.update(8, false), None);
        assert_eq!(d.update(7, false), Some(Alert::Critical(7))); // critical 而非 warn
        assert_eq!(d.update(6, false), None);
    }

    #[test]
    fn bounce_around_threshold_does_not_fire() {
        let mut d = ThresholdDebouncer::new(20, 10);
        for _ in 0..5 {
            assert_eq!(d.update(19, false), None);
            assert_eq!(d.update(21, false), None);
        }
    }

    #[test]
    fn falling_through_both_thresholds_escalates() {
        let mut d = ThresholdDebouncer::new(20, 10);
        d.update(15, false);
        assert_eq!(d.update(14, false), Some(Alert::Warn(14)));
        d.update(9, false);
        assert_eq!(d.update(8, false), Some(Alert::Critical(8))); // 升级
    }
}
