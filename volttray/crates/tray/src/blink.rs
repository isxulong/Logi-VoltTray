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
    /// 上一 tick 见到的开关状态（v0.3.14：检测「关→开」沿——用户显式
    /// 重新打开闪烁即明确的「恢复提醒」意图，必须清除停闪锁存；否则
    /// 「关闭闪烁 → 低电触发 → 重新打开」会因残留 ack 而永远不闪）。
    enabled_seen: bool,
}

impl BlinkState {
    pub fn new() -> Self {
        Self::default()
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

    /// 「关→开」沿清除停闪锁存（与 tick 内联逻辑同语义）。事件驱动泵
    /// 在非 tick 时机（菜单切换、配置热改镜像）调用：闪烁定时器只在
    /// 闪烁激活期间 arm，开关变化不能等下一个 tick 才生效（v0.3.15，
    /// P2-1②）。
    pub fn note_enabled(&mut self, enabled: bool) {
        if enabled && !self.enabled_seen {
            self.ack = false;
        }
        self.enabled_seen = enabled;
    }

    /// 500ms tick：翻转相位并报告本 tick 结果。
    /// `redraw` 仅在闪烁进行中为 true——常态时零重绘（不耗资源）。
    /// 开关「关→开」沿清除停闪锁存：重新打开闪烁 = 显式恢复提醒。
    pub fn tick(&mut self, enabled: bool) -> TickOutcome {
        self.note_enabled(enabled);
        self.phase = !self.phase;
        let blinking = self.is_blinking(enabled);
        TickOutcome {
            redraw: blinking,
            bright: blinking && self.phase,
            blinking,
        }
    }

    /// 不翻相位的状态查询（v0.3.15，P2-1②）：事件驱动泵在非 tick
    /// 时机（托盘事件、菜单、配置热改）需要知道「现在该不该重绘、
    /// 用哪一相」，但绝不能因此翻转相位——相位只能由 500ms 定时器
    /// 驱动（v0.3.14 #3 同族防线：tooltip 定时器不得驱动相位）。
    /// 同时镜像 note_enabled 的「关→开」沿清锁存语义。
    pub fn status(&mut self, enabled: bool) -> TickOutcome {
        self.note_enabled(enabled);
        let blinking = self.is_blinking(enabled);
        TickOutcome {
            redraw: blinking,
            bright: blinking && self.phase,
            blinking,
        }
    }

    /// 单击托盘停闪。返回：本次点击是否实际停掉了闪烁
    /// （非闪烁状态下的点击不重绘图标）。
    /// 调用方只允许传入「左键单击」（v0.3.14：右键是打开菜单——
    /// 用户开菜单恰恰是为了操作开关，若被当成停闪，则「低电中开菜单
    /// 重新打开闪烁」会永久压住本次低电期的闪烁）。
    pub fn acknowledge_click(&mut self) -> bool {
        if self.low_active && !self.ack {
            self.ack = true;
            true
        } else {
            false
        }
    }

    /// 当前是否处于低电活跃（v0.3.14：GUI 侧观测/日志用；设备灰显/
    /// 休眠期间调用方据此保持既有低电态，不冲掉停闪锁存——B3）。
    pub fn low_active(&self) -> bool {
        self.low_active
    }

    /// 停闪锁存是否生效（v0.3.14：闪烁启停日志的停闪原因判读用——B9）。
    pub fn acked(&self) -> bool {
        self.ack
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

    /// v0.3.15（P2-1②）：note_enabled 镜像「关→开」沿清除停闪锁存、
    /// 但不翻相位——事件驱动泵在定时器未 arm 时调用，相位只能由
    /// tick（500ms 定时器）推进。
    #[test]
    fn note_enabled_clears_ack_without_phase_flip() {
        let mut s = BlinkState::new();
        s.set_low_active(true);
        // 一个 tick：phase = true，闪烁中。
        let t = s.tick(true);
        assert!(t.blinking && t.bright, "first tick: phase=true → bright");
        // 单击停闪 → 锁存。
        assert!(s.acknowledge_click());
        assert!(!s.is_blinking(true));
        // 用户关→开（事件时机，非 tick）：ack 清除，立即恢复闪烁；
        // 相位未被推进（bright 仍为 true——若误翻相位会变 false）。
        s.note_enabled(false);
        s.note_enabled(true);
        assert!(s.is_blinking(true), "re-enable must clear ack");
        assert!(s.bright_frame(true), "note_enabled must not flip phase");
        // 开关保持开时 ack 不被镜像冲掉（与 tick 语义一致）。
        assert!(s.acknowledge_click());
        s.note_enabled(true);
        assert!(!s.is_blinking(true), "steady-on must keep ack");
    }

    /// v0.3.15（P2-1②）：status 供事件驱动泵查询当前相——返回与
    /// tick 同形的 TickOutcome，但绝不推进相位；反复调用结果稳定。
    #[test]
    fn status_reports_current_phase_without_advancing() {
        let mut s = BlinkState::new();
        s.set_low_active(true);
        let t = s.tick(true);
        assert!(t.bright, "first tick: phase=true");
        // 连续 status：相位不变（bright 恒 true）、重绘需求恒真。
        for _ in 0..8 {
            let st = s.status(true);
            assert!(st.blinking && st.redraw && st.bright);
        }
        // 下一个 tick 正常翻相（status 没有偷走相位）。
        let t2 = s.tick(true);
        assert!(!t2.bright, "second tick must flip to phase=false");
        // 非闪烁状态：status 报告不重绘。
        let mut idle = BlinkState::new();
        let st = idle.status(true);
        assert!(!st.blinking && !st.redraw);
    }

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

    /// v0.3.14（真机反馈 #2「关了再开不闪」）：停闪锁存后，开关
    /// 「关→开」沿必须清除 ack 并立即恢复闪烁；开关保持开则 ack 保持
    /// （单击停闪语义不被每 tick 意外冲掉）。
    #[test]
    fn reenable_clears_ack_and_resumes_blink() {
        let mut s = BlinkState::new();
        s.set_low_active(true);
        assert!(s.tick(true).blinking);
        // 单击停闪 → 锁存。
        assert!(s.acknowledge_click());
        assert!(!s.tick(true).blinking, "停闪后保持不闪");
        assert!(!s.tick(true).blinking, "开关保持开时 ack 不得被冲掉");
        // 用户关闭闪烁再重新打开（真机复现路径）→ 立即恢复闪烁。
        let _ = s.tick(false);
        let t = s.tick(true);
        assert!(
            t.blinking && t.redraw,
            "重新打开闪烁必须清除停闪锁存、立即恢复"
        );
        // 低电未解除时的「关→开」同样立即恢复。
        assert!(s.acknowledge_click());
        assert!(!s.tick(true).blinking);
        let _ = s.tick(false);
        assert!(s.tick(true).blinking);
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

    /// v0.3.5 #4 验收：透明形状 + 低电 → 闪烁状态机走「文本帧路径」。
    /// 状态机产出 bright 帧标记，渲染侧（IconCache 透明形状）对 bright 帧
    /// 施加文本颜色跳变 + 透明度脉冲，两帧必有可见差异；非透明形状维持
    /// 背景闪烁（背景帧差异、文本 alpha 恒 255）。
    /// 依赖 Windows 渲染侧（IconCache / GDI），仅 Windows 编译。
    #[cfg(windows)]
    #[test]
    fn transparent_low_battery_takes_text_frame_path() {
        use crate::icon::{IconCache, IconColor};
        use volttray_app::config::{
            pulse_alpha, IconBackground, IconFontColor, IconShape, BLINK_TEXT_ALPHA_PULSE,
        };

        let cache = IconCache::new(
            32,
            true,
            IconShape::Transparent,
            IconBackground::Dark,
            IconFontColor::Auto,
            "Segoe UI".to_string(),
            None,
            100,
            100,
        );
        let frame = |bright: bool| {
            cache
                .map_for_test(IconColor::Default, "15", bright)
                .expect("frame cached")
        };

        // 低电触发 → tick 出 bright=true 的一帧。
        let mut s = BlinkState::new();
        s.set_low_active(true);
        let (mut normal_tag, mut bright_tag) = (false, false);
        for _ in 0..4 {
            let t = s.tick(true);
            if t.bright {
                bright_tag = true;
            } else {
                normal_tag = true;
            }
        }
        assert!(normal_tag && bright_tag, "blink must alternate both frames");

        // 文本帧路径：bright 帧文本 alpha 被脉冲，normal 帧不脉冲。
        let (n, b) = (frame(false), frame(true));
        assert_ne!(n, b, "transparent blink frames must differ");
        let solid = n
            .chunks_exact(4)
            .position(|px| px[3] == 255)
            .expect("solid glyph pixel");
        assert_eq!(b[solid * 4 + 3], pulse_alpha(255, BLINK_TEXT_ALPHA_PULSE));
        // 背景闪烁路径不被误用：透明形状两帧都没有背景像素可闪，
        // 差异必须且只能落在文本像素上。
        for (pn, pb) in n.chunks_exact(4).zip(b.chunks_exact(4)) {
            if pn[3] == 0 {
                assert_eq!(pb[3], 0, "no background pixel may appear");
            }
        }
    }
}
