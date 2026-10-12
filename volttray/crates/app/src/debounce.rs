//! 单阈值低电追踪（v0.3.0 产品方案 M11/M12，取代 v0.2.0 的双阈值去抖）。
//!
//! 规则：
//! - 触发：连续 2 次读数 ≤ 阈值且设备未在充电；
//! - 解除（带 5% 迟滞）：读数 > 阈值 + 5，或设备进入充电；
//! - 阈值可通过 [`LowBatteryTracker::set_threshold`] 随配置热更新。

/// 告警类型：低于用户设定的低电阈值（携带本次百分比）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alert {
    /// 低电告警（含百分比）。
    LowBattery(u8),
}

/// 触发所需的连续越限次数。
pub const DEBOUNCE_COUNT: u8 = 2;

/// 解除所需的额外回升幅度（迟滞，百分比点）。
pub const RELEASE_HYSTERESIS: u8 = 5;

#[derive(Debug, Clone)]
pub struct LowBatteryTracker {
    threshold: u8,
    streak: u8,
    active: bool,
}

impl LowBatteryTracker {
    pub fn new(threshold: u8) -> Self {
        LowBatteryTracker {
            threshold,
            streak: 0,
            active: false,
        }
    }

    /// 配置热更新（设置窗口 / TOML 改阈值后调用）。
    pub fn set_threshold(&mut self, threshold: u8) {
        if threshold != self.threshold {
            self.threshold = threshold;
            // 阈值变化后重估：保守起见复位连击，已触发状态保留，
            // 由下一次 update 决定是否继续处于低电（活跃仅由迟滞/充电解除）。
            self.streak = 0;
        }
    }

    /// 喂入一次读数；返回本次应发的告警（若有）。
    pub fn update(&mut self, percent: u8, charging: bool) -> Option<Alert> {
        if charging {
            self.streak = 0;
            self.active = false;
            return None;
        }
        if self.active {
            // 迟滞解除：必须回升到阈值 + 5 之上。
            if percent > self.threshold.saturating_add(RELEASE_HYSTERESIS) {
                self.active = false;
                self.streak = 0;
            }
            return None;
        }
        if percent <= self.threshold {
            self.streak = self.streak.saturating_add(1);
            if self.streak >= DEBOUNCE_COUNT {
                self.active = true;
                self.streak = 0;
                return Some(Alert::LowBattery(percent));
            }
        } else {
            self.streak = 0;
        }
        None
    }

    /// 当前是否处于低电触发状态（托盘闪烁判定用）。
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// 当前生效阈值（测试与诊断用）。
    pub fn threshold(&self) -> u8 {
        self.threshold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_consecutive_at_or_below_fires_once() {
        let mut t = LowBatteryTracker::new(20);
        assert_eq!(t.update(21, false), None); // 高于阈值：不计连击
        assert_eq!(t.update(20, false), None); // 第 1 次（含等于）
        assert_eq!(t.update(18, false), Some(Alert::LowBattery(18))); // 第 2 次
        assert_eq!(t.update(17, false), None); // 已触发：不重复
        assert!(t.is_active());
    }

    #[test]
    fn release_requires_hysteresis() {
        let mut t = LowBatteryTracker::new(20);
        t.update(19, false);
        t.update(19, false);
        assert!(t.is_active());
        assert_eq!(t.update(21, false), None); // 回到阈值之上但不足迟滞
        assert!(t.is_active());
        assert_eq!(t.update(26, false), None); // > 20+5 才解除
        assert!(!t.is_active());
    }

    #[test]
    fn release_then_can_fire_again() {
        let mut t = LowBatteryTracker::new(20);
        t.update(15, false);
        t.update(15, false);
        assert!(t.is_active());
        t.update(26, false); // 迟滞解除
        assert!(!t.is_active());
        assert_eq!(t.update(19, false), None);
        assert_eq!(t.update(19, false), Some(Alert::LowBattery(19)));
    }

    #[test]
    fn charging_releases_immediately() {
        let mut t = LowBatteryTracker::new(20);
        t.update(15, false);
        t.update(15, false);
        assert!(t.is_active());
        t.update(10, true); // 充电：立即解除
        assert!(!t.is_active());
        assert_eq!(t.update(15, false), None); // 连击已复位
    }

    #[test]
    fn bounce_around_threshold_does_not_fire() {
        let mut t = LowBatteryTracker::new(20);
        for _ in 0..5 {
            assert_eq!(t.update(19, false), None);
            assert_eq!(t.update(21, false), None);
        }
        assert!(!t.is_active());
    }

    #[test]
    fn set_threshold_takes_effect() {
        let mut t = LowBatteryTracker::new(20);
        assert_eq!(t.threshold(), 20);
        t.set_threshold(50);
        assert_eq!(t.threshold(), 50);
        assert_eq!(t.update(49, false), None);
        assert_eq!(t.update(49, false), Some(Alert::LowBattery(49)));
        // 相同值不重置连击
        t.set_threshold(50);
        assert_eq!(t.update(60, false), None);
    }
}
