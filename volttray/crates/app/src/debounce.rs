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

    /// 配置变更后的即时重估（v0.3.14 修复：设置改动立即生效）。
    ///
    /// 以上一次已知读数为准立即重判低电态，不等下一次轮询读数
    /// （默认轮询 1800s：改阈值后旧逻辑最长要等 2 个轮询周期、即约 1 小时
    /// 才能在闪烁/通知上体现，用户因此判定「改动不生效」）。迟滞只用于
    /// 抑制传感读数抖动，不用于用户显式改配置：阈值调到电量之上 → 立即
    /// 进入低电（返回告警）；调到电量之下 → 立即解除。纯主机侧状态重估，
    /// 零射频，不违反休眠设备零主动查询红线。
    pub fn evaluate_now(&mut self, percent: u8, charging: bool) -> Option<Alert> {
        self.streak = 0;
        if charging || percent > self.threshold {
            self.active = false;
            return None;
        }
        if self.active {
            return None;
        }
        self.active = true;
        Some(Alert::LowBattery(percent))
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

    /// v0.3.14：阈值调到电量之上 → evaluate_now 立即进入低电并告警一次；
    /// 重复重估不重复告警。
    #[test]
    fn evaluate_now_activates_immediately_when_threshold_raised() {
        let mut t = LowBatteryTracker::new(20);
        t.update(30, false);
        assert!(!t.is_active()); // 30 > 20：未触发
        t.set_threshold(35); // 用户把阈值调到电量之上
        assert_eq!(
            t.evaluate_now(30, false),
            Some(Alert::LowBattery(30)),
            "阈值调高后必须立即按最后已知电量进入低电"
        );
        assert!(t.is_active());
        // 同一低电期内的再次重估不重复告警。
        assert_eq!(t.evaluate_now(30, false), None);
    }

    /// v0.3.14：阈值调到电量之下 → evaluate_now 立即解除（不等下一次
    /// 轮询读数走迟滞路径——默认轮询档下那是 30 分钟量级的延迟）。
    #[test]
    fn evaluate_now_releases_immediately_when_threshold_lowered() {
        let mut t = LowBatteryTracker::new(30);
        t.update(25, false);
        t.update(25, false);
        assert!(t.is_active());
        t.set_threshold(20); // 用户把阈值调到电量（25）之下
        assert_eq!(t.evaluate_now(25, false), None, "解除不产生告警");
        assert!(!t.is_active(), "阈值调低后必须立即解除低电态");
        // 解除后恢复常规去抖：需连续 2 次越限才重新触发。
        assert_eq!(t.update(19, false), None);
        assert_eq!(t.update(19, false), Some(Alert::LowBattery(19)));
    }

    /// v0.3.14：充电中重估一律解除（D-30：充电设备不提醒）。
    #[test]
    fn evaluate_now_releases_when_charging() {
        let mut t = LowBatteryTracker::new(40);
        t.update(30, false);
        t.update(30, false);
        assert!(t.is_active());
        assert_eq!(t.evaluate_now(30, true), None);
        assert!(!t.is_active());
    }
}
