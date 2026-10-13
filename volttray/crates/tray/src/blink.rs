//! 低电闪烁状态机（M11；v0.3.4 #3 从 main.rs 内联逻辑抽出成可测模块）。
//!
//! 根因（真机 #3，D-46）：v0.3.3 的「提亮帧」用乘法提亮（v×k）——饱和通道
//! 被钳回原值（`brighten((255,0,0),0.4)` 还是 `(255,0,0)`），暗色通道变化
//! 又低于感知阈值，导致常态帧与提亮帧**逐像素相同或差异不可见**，闪烁
//! 「看起来没生效」。相位翻转、重绘调度本身是工作的，坏在两帧无差异。
//! 修复在渲染侧：提亮帧改用 `flash_rgb`（向白插值；近白颜色向深灰插值，
//! 保证任意颜色两帧必有可感知差异，见 config.rs）。本模块负责状态机本身：
//! 相位、低电活跃、单击停闪、解除重新武装。

/// 500ms tick 一次的相位翻转结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TickOutcome {
    /// 本 tick 是否需要重绘（闪烁进行中时为 true）。
    pub redraw: bool,
    /// 本 tick 是否取「提亮帧」。
    pub bright: bool,
    /// 闪烁是否进行中。
    pub blinking: bool,
}

/// 低电闪烁状态机。
#[derive(Debug, Clone, Copy, Default)]
pub struct BlinkState {
    /// 相位（500ms 翻转 → 1Hz）：false = 常态帧，true = 提亮帧。
    phase: bool,
    /// 展示设备当前处于低电触发状态（由调用方每 tick 喂入）。
    low_active: bool,
    /// 用户已单击停闪（本次低电期间保持；低电解除后重新武装）。
    ack: bool,
}

impl BlinkState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前是否处于低电触发状态。
    pub fn is_low_active(&self) -> bool {
        self.low_active
    }

    /// 闪烁是否进行中（含「闪烁开关」判定，开关由调用方持有）。
    pub fn is_blinking(&self, enabled: bool) -> bool {
        enabled && self.low_active && !self.ack
    }

    /// 当前帧是否为提亮帧（refresh_ui 取帧用）。
    pub fn bright_frame(&self, enabled: bool) -> bool {
        self.is_blinking(enabled) && self.phase
    }

    /// 喂入最新低电判定（充电/阈值恢复/设备切换都会改变它）。
    /// 返回：可视化状态是否发生变化（true = 调用方需要重绘）。
    /// 低电解除时清 ack——下次低电重新武装（M11 原设计）。
    pub fn set_low_active(&mut self, low: bool) -> bool {
        if low == self.low_active {
            return false;
        }
        self.low_active = low;
        if !low {
            self.ack = false;
        }
        true
    }

    /// 500ms tick：翻转相位并报告本 tick 结果。
    /// `redraw` 仅在闪烁进行中为 true——常态时零重绘（不耗资源）。
    pub fn tick(&mut self, enabled: bool) -> TickOutcome {
        self.phase = !self.phase;
        let blinking = self.is_blinking(enabled);
        TickOutcome {
            redraw: blinking,
            bright: blinking && self.phase,
            blinking,
        }
    }

    /// 单击托盘停闪。返回：本次点击是否实际停掉了闪烁
    /// （非闪烁状态下的点击不重绘图标）。
    pub fn acknowledge_click(&mut self) -> bool {
        if self.low_active && !self.ack {
            self.ack = true;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, RwLock};
    use std::time::Instant;
    use volttray_app::{AppCore, Config};
    use volttray_device::battery::{BatteryReading, BatterySource};
    use volttray_device::LinkKind;

    /// 低电 → tick 进入 blink 且调度重绘；相位逐 tick 交替。
    #[test]
    fn low_battery_enters_blink_and_schedules_repaint() {
        let mut s = BlinkState::new();
        assert!(s.set_low_active(true), "low activation must signal change");
        let t1 = s.tick(true);
        assert!(t1.blinking, "must be blinking when low + enabled");
        assert!(t1.redraw, "blink tick must schedule repaint");
        // 相位交替：提亮/常态两帧轮换。
        let t2 = s.tick(true);
        assert!(t2.blinking && t2.redraw);
        assert_ne!(t1.bright, t2.bright, "bright frame must alternate");
        // 常态（非低电）不重绘。
        let mut idle = BlinkState::new();
        let t0 = idle.tick(true);
        assert!(!t0.blinking && !t0.redraw);
    }

    /// 单击停闪：ack 后不再 blink；低电解除 → 重新武装。
    #[test]
    fn click_stops_blink_and_rearms_on_release() {
        let mut s = BlinkState::new();
        s.set_low_active(true);
        assert!(s.tick(true).blinking);
        // 单击 → 停。
        assert!(s.acknowledge_click(), "click during blink must stop it");
        let t = s.tick(true);
        assert!(!t.blinking && !t.redraw, "no blink after click");
        assert!(!s.bright_frame(true));
        // 低电解除 → ack 复位。
        assert!(s.set_low_active(false));
        // 再次低电 → 恢复闪烁。
        assert!(s.set_low_active(true));
        assert!(s.tick(true).blinking, "blink must re-arm after release");
    }

    /// 闪烁开关关闭：低电也不闪；开启立即恢复。
    #[test]
    fn disabled_switch_suppresses_blink() {
        let mut s = BlinkState::new();
        s.set_low_active(true);
        let t = s.tick(false);
        assert!(!t.blinking && !t.redraw && !t.bright);
        assert!(!s.bright_frame(false));
        let t2 = s.tick(true);
        assert!(t2.blinking, "re-enabling must resume blink");
    }

    /// 非闪烁状态的单击不应误触发重绘/改状态。
    #[test]
    fn click_outside_blink_is_noop() {
        let mut s = BlinkState::new();
        assert!(!s.acknowledge_click());
        s.set_low_active(true);
        s.acknowledge_click();
        // 已 ack 的再次点击不再有副作用。
        assert!(!s.acknowledge_click());
    }

    /// 集成（真机 #3 验收的判定链路）：模拟电量低于阈值两次（去抖）→
    /// AppCore 判低电 → 状态机进入 blink；充电中（D-30 设计：充电设备
    /// 外部供电，不提醒不闪烁）与电量恢复（回滞 5%）→ 解除 → 状态机停止。
    #[test]
    fn appcore_low_battery_drives_blink_lifecycle() {
        let cfg = Arc::new(RwLock::new(Config::default()));
        let mut core = AppCore::new(cfg);
        core.upsert_device(volttray_device::Device::new(
            "m1".into(),
            "MX anywhere test",
            LinkKind::Direct,
            1,
            false,
        ));
        let now = Instant::now();
        let read = |pct: u8, charging: bool| BatteryReading {
            percent: pct,
            charging,
            source: BatterySource::UnifiedBattery,
        };
        let mut blink = BlinkState::new();
        // 去抖：DEBOUNCE_COUNT=2 次低读数才触发。
        core.record_success("m1", read(15, false), now);
        core.record_success("m1", read(15, false), now);
        assert!(
            core.is_low_battery("m1"),
            "two low readings must arm low battery"
        );
        assert!(blink.set_low_active(core.is_low_battery("m1")));
        assert!(blink.tick(true).blinking, "low battery must drive blink");
        // 充电：tracker 立即解除（充电设备不闪烁，D-30）。
        core.record_success("m1", read(15, true), now);
        assert!(
            !core.is_low_battery("m1"),
            "charging must release low battery"
        );
        assert!(blink.set_low_active(core.is_low_battery("m1")));
        let t = blink.tick(true);
        assert!(!t.blinking && !t.redraw, "charging release must stop blink");
        // 高于阈值 + 回滞 → 解除后重新读取也不再触发。
        core.record_success("m1", read(90, false), now);
        core.record_success("m1", read(90, false), now);
        assert!(!core.is_low_battery("m1"));
    }
}
