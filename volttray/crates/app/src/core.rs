//! 应用核心：设备台账、快照、默认展示设备、90s 灰显派生、调度节奏。
//!
//! 与传输层解耦：本模块只维护状态与纯逻辑决策，全部桩上可测。

use crate::config::Config;
use crate::debounce::{Alert, LowBatteryTracker};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use volttray_device::battery::BatteryReading;
use volttray_device::state::{mark_online, DeviceState};
use volttray_device::Device;

/// 灰显派生窗口（PRD M2：90s 无成功响应）。
pub const GRAY_AFTER: Duration = Duration::from_secs(90);
/// 充电时的轮询间隔（蓝图：15s）。
pub const POLL_CHARGING: Duration = Duration::from_secs(15);
/// 连续多少次查询超时判定设备进入休眠（D-33：超时 = 疑似休眠，
/// 暂停主动轮询而不是累计失败注销——对休眠中的鼠标发查询才是真费电）。
pub const SLEEP_AFTER_TIMEOUTS: u32 = 2;
/// 唤醒探测冷却：一次唤醒信号至多触发一次探测；探测失败后须等待
/// 新的唤醒信号、且距上次探测 ≥ 该间隔，避免唤醒信号风暴变成高频查询。
pub const WAKE_PROBE_COOLDOWN: Duration = Duration::from_secs(5);

/// 轮询决策（D-33「休眠期零主动查询」的唯一裁决点：
/// Skip = 本轮绝不向该设备发出任何 HID++ 查询）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollDecision {
    Probe,
    Skip,
}

/// `record_timeout` 的结果（IO 线程据此发 StateChange 诊断事件）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeoutOutcome {
    /// 仍在线，已连续 N 次超时（未达休眠阈值）。
    Counting(u32),
    /// 本次超时使设备进入休眠（Online → Sleeping）。
    EnteredSleep,
    /// 已处于休眠（唤醒探测再次超时，保持休眠、不计失败）。
    AlreadySleeping,
}

/// 单台设备的运行时台账。
#[derive(Debug, Clone)]
pub struct DeviceEntry {
    pub device: Device,
    pub last_success: Option<Instant>,
    gray_alerted: bool,
    /// 连续查询超时次数（成功即清零；D-33）。
    timeout_streak: u32,
    /// 收到唤醒信号、等待一次唤醒探测（D-33 被动唤醒）。
    wake_armed: bool,
    /// 最近一次唤醒探测的时刻（冷却窗口判定）。
    last_wake_probe: Option<Instant>,
}

impl DeviceEntry {
    pub fn new(device: Device) -> Self {
        DeviceEntry {
            device,
            last_success: None,
            gray_alerted: false,
            timeout_streak: 0,
            wake_armed: false,
            last_wake_probe: None,
        }
    }

    /// 派生灰显态（90s 无成功；从未成功过的在线设备不灰）。
    pub fn is_gray(&self, now: Instant) -> bool {
        match self.last_success {
            Some(t) => now.duration_since(t) >= GRAY_AFTER,
            None => false,
        }
    }

    /// 展示用灰显（D-33）：90s 派生灰，或休眠中——休眠显示最后一次
    /// 已知电量并灰显，不显示成「未读取 / 未知」。
    pub fn display_gray(&self, now: Instant) -> bool {
        self.device.state == DeviceState::Sleeping || self.is_gray(now)
    }
}

/// 给 UI 的设备快照行。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DeviceSnapshot {
    pub id: String,
    pub name: String,
    pub percent: Option<u8>,
    pub charging: bool,
    pub link: String,
    pub state: String,
    pub gray: bool,
    pub source: Option<String>,
}

/// 给 UI 的整体快照。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Snapshot {
    pub seq: u64,
    pub devices: Vec<DeviceSnapshot>,
    /// 托盘图标默认展示的设备下标（None = 无可用电量数据）。
    pub default_device: Option<usize>,
}

#[derive(Debug)]
pub struct AppCore {
    config: Arc<RwLock<Config>>,
    entries: Vec<DeviceEntry>,
    lowbats: HashMap<String, LowBatteryTracker>,
    snap_seq: u64,
}

impl AppCore {
    pub fn new(config: Arc<RwLock<Config>>) -> Self {
        AppCore {
            config,
            entries: Vec::new(),
            lowbats: HashMap::new(),
            snap_seq: 0,
        }
    }

    pub fn config(&self) -> Config {
        self.config.read().map(|c| c.clone()).unwrap_or_default()
    }

    /// 原子换入新配置（P1-2：配置所有权在 app）。
    pub fn swap_config(&self, next: Config) {
        if let Ok(mut guard) = self.config.write() {
            *guard = next;
        }
    }

    pub fn upsert_device(&mut self, device: Device) {
        match self.entries.iter_mut().find(|e| e.device.id == device.id) {
            Some(e) => {
                let battery = e.device.battery;
                let state = e.device.state;
                e.device = device;
                e.device.battery = battery;
                e.device.state = state;
            }
            None => self.entries.push(DeviceEntry::new(device)),
        }
    }

    pub fn remove_device(&mut self, id: &str) {
        self.entries.retain(|e| e.device.id != id);
        self.lowbats.remove(id);
    }

    pub fn entries(&self) -> &[DeviceEntry] {
        &self.entries
    }

    /// 记录一次成功电量读取；返回去抖后应发的低电告警（若有）。
    ///
    /// D-33：成功即恢复——超时连击清零、唤醒臂解除，状态经 `mark_online`
    /// 迁回 Online（覆盖 Sleeping 唤醒恢复、以及重新注册时台账里残留的
    /// Disconnected / Connected 旧态——v0.3.0 的 upsert 会保留旧 state）。
    pub fn record_success(
        &mut self,
        id: &str,
        reading: BatteryReading,
        now: Instant,
    ) -> Option<Alert> {
        let entry = self.entries.iter_mut().find(|e| e.device.id == id)?;
        entry.last_success = Some(now);
        entry.gray_alerted = false;
        entry.timeout_streak = 0;
        entry.wake_armed = false;
        entry.last_wake_probe = None;
        entry.device.state = mark_online(entry.device.state);
        entry.device.battery = Some(reading);
        let cfg = self.config();
        let tracker = self
            .lowbats
            .entry(id.to_string())
            .or_insert_with(|| LowBatteryTracker::new(cfg.low_threshold));
        // 阈值热更新（设置窗口 / TOML 改动均经共享配置生效）。
        tracker.set_threshold(cfg.low_threshold);
        tracker.update(reading.percent, reading.charging)
    }

    /// 指定设备当前是否处于低电触发状态（M11 托盘闪烁判定用）。
    pub fn is_low_battery(&self, id: &str) -> bool {
        self.lowbats.get(id).is_some_and(|t| t.is_active())
    }

    /// 记录一次查询超时（D-33）：连续 `SLEEP_AFTER_TIMEOUTS` 次超时 →
    /// Online → Sleeping，此后 `poll_decision` 一律 Skip（零主动查询），
    /// 直到被动唤醒信号到达。休眠中的再次超时（唤醒探测失败）保持休眠。
    pub fn record_timeout(&mut self, id: &str) -> TimeoutOutcome {
        let Some(entry) = self.entries.iter_mut().find(|e| e.device.id == id) else {
            return TimeoutOutcome::Counting(0);
        };
        entry.timeout_streak = entry.timeout_streak.saturating_add(1);
        match entry.device.state {
            DeviceState::Sleeping => TimeoutOutcome::AlreadySleeping,
            DeviceState::Online if entry.timeout_streak >= SLEEP_AFTER_TIMEOUTS => {
                entry.device.set_state(DeviceState::Sleeping);
                entry.wake_armed = false;
                entry.last_wake_probe = None;
                TimeoutOutcome::EnteredSleep
            }
            // Connected / Disconnected 下只计数、不改态
            //（Disconnected 由 gone 路径管理，GrayedOut 不落库）。
            _ => TimeoutOutcome::Counting(entry.timeout_streak),
        }
    }

    /// 被动唤醒信号（D-33）：系统层观测到鼠标活动（Raw Input）→ 给休眠
    /// 设备装填一次唤醒探测资格。在线设备忽略（正常轮询节奏不变）。
    pub fn note_wake_signal(&mut self, id: &str) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.device.id == id) {
            if entry.device.state == DeviceState::Sleeping {
                entry.wake_armed = true;
            }
        }
    }

    /// 当前处于休眠的设备 id 列表（IO 线程收到唤醒信号后批量装填用）。
    pub fn sleeping_ids(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|e| e.device.state == DeviceState::Sleeping)
            .map(|e| e.device.id.clone())
            .collect()
    }

    /// 一次唤醒探测开始（D-33）：消耗唤醒资格并进入冷却——本次探测
    /// 无论成败，都要等下一个唤醒信号 + 冷却窗口过后才会再探测。
    pub fn begin_wake_probe(&mut self, id: &str, now: Instant) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.device.id == id) {
            entry.wake_armed = false;
            entry.last_wake_probe = Some(now);
        }
    }

    /// 本轮是否向该设备发查询（D-33 零主动查询裁决点）：
    /// 非休眠一律 Probe；休眠设备仅当「唤醒资格已装填且已过冷却」才 Probe
    /// 一次，其余一律 Skip——休眠期间绝不发出主动查询。
    pub fn poll_decision(&self, id: &str, now: Instant) -> PollDecision {
        let Some(entry) = self.entries.iter().find(|e| e.device.id == id) else {
            return PollDecision::Probe;
        };
        if entry.device.state != DeviceState::Sleeping {
            return PollDecision::Probe;
        }
        let cooled = entry
            .last_wake_probe
            .is_none_or(|t| now.duration_since(t) >= WAKE_PROBE_COOLDOWN);
        if entry.wake_armed && cooled {
            PollDecision::Probe
        } else {
            PollDecision::Skip
        }
    }

    /// 记录一次失败（gone=true 表示设备消失）。
    pub fn record_failure(&mut self, id: &str, gone: bool) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.device.id == id) {
            if gone && entry.device.state != DeviceState::Disconnected {
                entry.timeout_streak = 0;
                entry.wake_armed = false;
                entry.last_wake_probe = None;
                let to = DeviceState::Disconnected;
                entry.device.set_state(to);
            }
        }
    }

    /// 检查是否有设备新进入灰显（用于"90s 设备丢失"通知）；返回新变灰的设备名。
    pub fn newly_gray(&mut self, now: Instant) -> Vec<String> {
        let mut out = Vec::new();
        for e in &mut self.entries {
            if e.device.state == DeviceState::Online && e.is_gray(now) && !e.gray_alerted {
                e.gray_alerted = true;
                out.push(e.device.name.clone());
            }
        }
        out
    }

    /// 默认展示设备（决策 D-08，蓝图缺口的就地裁决）：
    /// 在「有电量、未充电、未灰显」的设备中取电量最低者；无候选则
    /// 放宽到「有电量、未灰显」取最低；仍无 → None（图标显示 "?"）。
    /// D-33：休眠设备保留展示资格（显示最后一次已知电量、灰显），
    /// 不被 90s 派生灰显排除——否则唯一设备休眠后图标又回退「?」。
    pub fn default_display_index(&self, now: Instant) -> Option<usize> {
        let eligible = |e: &DeviceEntry| e.device.state == DeviceState::Sleeping || !e.is_gray(now);
        let pick_min = |pred: &dyn Fn(&DeviceEntry) -> bool| -> Option<usize> {
            self.entries
                .iter()
                .enumerate()
                .filter(|(_, e)| pred(e))
                .filter_map(|(i, e)| e.device.battery.map(|b| (i, b.percent)))
                .min_by_key(|(_, p)| *p)
                .map(|(i, _)| i)
        };
        pick_min(&|e| {
            e.device.battery.is_some()
                && !e.device.battery.is_some_and(|b| b.charging)
                && eligible(e)
        })
        .or_else(|| pick_min(&|e| e.device.battery.is_some() && eligible(e)))
    }

    /// 下一次轮询间隔：充电中 15s，否则按配置。
    pub fn poll_interval(&self) -> Duration {
        let any_charging = self
            .entries
            .iter()
            .any(|e| e.device.battery.is_some_and(|b| b.charging));
        if any_charging {
            POLL_CHARGING
        } else {
            Duration::from_secs(self.config().poll_interval_secs)
        }
    }

    /// 生成快照。
    pub fn snapshot(&mut self, now: Instant) -> Snapshot {
        self.snap_seq += 1;
        let default_device = self.default_display_index(now);
        Snapshot {
            seq: self.snap_seq,
            devices: self
                .entries
                .iter()
                .map(|e| DeviceSnapshot {
                    id: e.device.id.clone(),
                    name: e.device.name.clone(),
                    percent: e.device.battery.map(|b| b.percent),
                    charging: e.device.battery.is_some_and(|b| b.charging),
                    link: e.device.link.name().to_string(),
                    state: e.device.state.name().to_string(),
                    gray: e.display_gray(now),
                    source: e.device.battery.map(|b| b.source.name().to_string()),
                })
                .collect(),
            default_device,
        }
    }
}

/// GUI 边界的 panic 护栏（PRD：catch_unwind）。
pub fn guarded<F>(what: &str, f: F) -> Result<(), String>
where
    F: FnOnce() + std::panic::UnwindSafe,
{
    std::panic::catch_unwind(f).map_err(|_| format!("{what} panicked"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use volttray_device::battery::BatterySource;
    use volttray_device::LinkKind;

    fn reading(percent: u8, charging: bool) -> BatteryReading {
        BatteryReading {
            percent,
            charging,
            source: BatterySource::UnifiedBattery,
        }
    }

    fn dev(id: &str, index: u8) -> Device {
        let mut d = Device::new(
            id.into(),
            &format!("Mouse {id}"),
            LinkKind::Bolt,
            index,
            false,
        );
        d.set_state(DeviceState::Online);
        d
    }

    #[test]
    fn default_display_picks_lowest_non_charging() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let now = Instant::now();
        core.upsert_device(dev("a", 1));
        core.upsert_device(dev("b", 2));
        core.upsert_device(dev("c", 3));
        core.record_success("a", reading(50, false), now);
        core.record_success("b", reading(15, false), now);
        core.record_success("c", reading(10, true), now); // 充电中：排除
        assert_eq!(core.default_display_index(now), Some(1)); // b=15%
    }

    #[test]
    fn gray_devices_excluded_from_default() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let t0 = Instant::now();
        core.upsert_device(dev("a", 1));
        core.record_success("a", reading(5, false), t0);
        let later = t0 + GRAY_AFTER + Duration::from_secs(1);
        assert_eq!(core.default_display_index(later), None); // 唯一设备已灰
    }

    #[test]
    fn newly_gray_fires_once_and_recovers() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let t0 = Instant::now();
        core.upsert_device(dev("a", 1));
        core.record_success("a", reading(80, false), t0);
        let later = t0 + GRAY_AFTER + Duration::from_secs(1);
        assert_eq!(core.newly_gray(later), vec!["Mouse a".to_string()]);
        assert!(core.newly_gray(later).is_empty()); // 不重复
        core.record_success("a", reading(80, false), later);
        assert!(core.newly_gray(later).is_empty());
    }

    #[test]
    fn poll_interval_switches_when_charging() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let now = Instant::now();
        core.upsert_device(dev("a", 1));
        assert_eq!(core.poll_interval(), Duration::from_secs(60));
        core.record_success("a", reading(30, true), now);
        assert_eq!(core.poll_interval(), POLL_CHARGING);
    }

    #[test]
    fn snapshot_reflects_devices() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let now = Instant::now();
        core.upsert_device(dev("a", 1));
        core.record_success("a", reading(66, false), now);
        let snap = core.snapshot(now);
        assert_eq!(snap.seq, 1);
        assert_eq!(snap.devices.len(), 1);
        assert_eq!(snap.devices[0].percent, Some(66));
        assert_eq!(snap.default_device, Some(0));
    }

    #[test]
    fn threshold_alert_flows_through_record_success() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let now = Instant::now();
        core.upsert_device(dev("a", 1));
        assert_eq!(core.record_success("a", reading(19, false), now), None);
        assert_eq!(
            core.record_success("a", reading(18, false), now),
            Some(Alert::LowBattery(18))
        );
        assert!(core.is_low_battery("a"));
    }

    #[test]
    fn low_threshold_hot_update_via_shared_config() {
        let cfg = Arc::new(RwLock::new(Config::default()));
        let mut core = AppCore::new(cfg.clone());
        let now = Instant::now();
        core.upsert_device(dev("a", 1));
        core.record_success("a", reading(30, false), now);
        core.record_success("a", reading(30, false), now);
        assert!(!core.is_low_battery("a")); // 30 > 默认阈值 20
        let next = Config {
            low_threshold: 35,
            ..Config::default()
        };
        core.swap_config(next);
        // 阈值热更新复位连击（debounce::set_threshold 保守复位），需再连续 2 次越限才触发（M11）。
        assert_eq!(core.record_success("a", reading(30, false), now), None);
        assert_eq!(
            core.record_success("a", reading(30, false), now),
            Some(Alert::LowBattery(30))
        );
        assert!(core.is_low_battery("a"));
    }

    #[test]
    fn charging_stops_low_battery_state() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let now = Instant::now();
        core.upsert_device(dev("a", 1));
        core.record_success("a", reading(10, false), now);
        core.record_success("a", reading(10, false), now);
        assert!(core.is_low_battery("a"));
        core.record_success("a", reading(10, true), now);
        assert!(!core.is_low_battery("a"));
    }

    #[test]
    fn guarded_catches_panic() {
        let r = guarded("boom", || panic!("x"));
        assert!(r.is_err());
        let ok = guarded("fine", || {});
        assert!(ok.is_ok());
    }

    // —— D-33 休眠/唤醒状态机（被动、零额外耗电）——

    fn sleeping_core(id: &str) -> (AppCore, Instant) {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let now = Instant::now();
        core.upsert_device(dev(id, 1));
        core.record_success(id, reading(66, false), now);
        // 连续 SLEEP_AFTER_TIMEOUTS 次查询超时 → 进入休眠。
        assert_eq!(core.record_timeout(id), TimeoutOutcome::Counting(1));
        assert_eq!(core.record_timeout(id), TimeoutOutcome::EnteredSleep);
        (core, now)
    }

    /// 连续查询超时 → Online → Sleeping；再次超时保持休眠（不 panic）。
    #[test]
    fn consecutive_timeouts_enter_sleep() {
        let (mut core, _now) = sleeping_core("a");
        let e = &core.entries()[0];
        assert_eq!(e.device.state, DeviceState::Sleeping);
        assert_eq!(core.record_timeout("a"), TimeoutOutcome::AlreadySleeping);
        assert_eq!(core.entries()[0].device.state, DeviceState::Sleeping);
    }

    /// 核心断言：休眠期间零主动查询——任意多个轮询 tick 全部 Skip。
    #[test]
    fn zero_queries_while_sleeping() {
        let (core, now) = sleeping_core("a");
        for tick in 0u64..100 {
            let t = now + Duration::from_secs(60 * tick);
            assert_eq!(
                core.poll_decision("a", t),
                PollDecision::Skip,
                "tick {tick}: 休眠中不得发出主动查询"
            );
        }
    }

    /// 唤醒信号 → 装填一次探测资格；探测开始即消耗并进入冷却；
    /// 冷却过后没有新信号仍然 Skip（不得自行恢复主动轮询）。
    #[test]
    fn wake_signal_arms_exactly_one_probe() {
        let (mut core, now) = sleeping_core("a");
        // 无信号：Skip。
        assert_eq!(core.poll_decision("a", now), PollDecision::Skip);
        // 被动事件（系统观测到鼠标活动）→ 一次探测资格。
        core.note_wake_signal("a");
        assert_eq!(core.poll_decision("a", now), PollDecision::Probe);
        // 探测开始：资格消耗 + 冷却开始。
        core.begin_wake_probe("a", now);
        assert_eq!(core.poll_decision("a", now), PollDecision::Skip);
        // 冷却窗口内即使再来信号也不立即探测（防信号风暴 = 变相高频查询）。
        core.note_wake_signal("a");
        let within = now + WAKE_PROBE_COOLDOWN - Duration::from_secs(1);
        assert_eq!(core.poll_decision("a", within), PollDecision::Skip);
        // 冷却过后、且持有新信号资格 → 再探测一次。
        let after = now + WAKE_PROBE_COOLDOWN;
        assert_eq!(core.poll_decision("a", after), PollDecision::Probe);
        // 若没有新信号，冷却过后依然 Skip。
        core.begin_wake_probe("a", after);
        let later = after + WAKE_PROBE_COOLDOWN + Duration::from_secs(3600);
        assert_eq!(core.poll_decision("a", later), PollDecision::Skip);
    }

    /// 休眠 → 唤醒探测成功 → Online，恢复正常轮询节奏。
    #[test]
    fn wake_probe_success_restores_online() {
        let (mut core, now) = sleeping_core("a");
        core.note_wake_signal("a");
        assert_eq!(core.poll_decision("a", now), PollDecision::Probe);
        core.begin_wake_probe("a", now);
        core.record_success("a", reading(70, false), now);
        assert_eq!(core.entries()[0].device.state, DeviceState::Online);
        assert_eq!(core.poll_decision("a", now), PollDecision::Probe);
        assert_eq!(core.sleeping_ids(), Vec::<String>::new());
    }

    /// 休眠 → 唤醒探测超时 → 保持休眠，等待下一次唤醒信号（不注销、
    /// 不自行重试——对休眠鼠标发查询才是真费电）。
    #[test]
    fn wake_probe_timeout_stays_sleeping() {
        let (mut core, now) = sleeping_core("a");
        core.note_wake_signal("a");
        core.begin_wake_probe("a", now);
        assert_eq!(core.record_timeout("a"), TimeoutOutcome::AlreadySleeping);
        assert_eq!(core.entries()[0].device.state, DeviceState::Sleeping);
        let later = now + Duration::from_secs(3600);
        assert_eq!(core.poll_decision("a", later), PollDecision::Skip);
    }

    /// 休眠展示：最后一次已知电量 + 灰显 + state=sleeping，
    /// 不显示成「未读取/未知」；默认展示设备资格保留（唯一设备休眠
    /// 时图标仍显示最后已知值，不回退「?」）。
    #[test]
    fn sleeping_shows_last_known_percent_grayed() {
        let (mut core, now) = sleeping_core("a");
        let much_later = now + GRAY_AFTER + Duration::from_secs(600);
        let snap = core.snapshot(much_later);
        assert_eq!(snap.devices[0].percent, Some(66));
        assert_eq!(snap.devices[0].state, "sleeping");
        assert!(snap.devices[0].gray, "休眠应灰显");
        assert_eq!(
            snap.default_device,
            Some(0),
            "休眠设备保留展示资格（显示最后已知电量）"
        );
    }

    /// 休眠不触发「90s 设备离线」告警（休眠是合法状态，不是丢失）。
    #[test]
    fn sleeping_does_not_fire_gray_alert() {
        let (mut core, now) = sleeping_core("a");
        let much_later = now + GRAY_AFTER + Duration::from_secs(600);
        assert!(core.newly_gray(much_later).is_empty());
    }

    /// 台账残留旧态修复：Disconnected / Connected 的设备一旦读数成功，
    /// 一律经 mark_online 迁回 Online（v0.3.0 upsert 保留旧 state 的滞留 bug）。
    #[test]
    fn record_success_clears_stale_state() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let now = Instant::now();
        core.upsert_device(dev("a", 1));
        // Connected → 成功 → Online
        assert_eq!(core.entries()[0].device.state, DeviceState::Connected);
        core.record_success("a", reading(50, false), now);
        assert_eq!(core.entries()[0].device.state, DeviceState::Online);
        // Disconnected（gone 路径）→ 重新注册后成功 → Online
        core.record_failure("a", true);
        assert_eq!(core.entries()[0].device.state, DeviceState::Disconnected);
        core.record_success("a", reading(50, false), now);
        assert_eq!(core.entries()[0].device.state, DeviceState::Online);
    }

    /// 休眠中通道报 DeviceGone → Disconnected（接收器拔出等真消失）。
    #[test]
    fn sleeping_device_gone_disconnects() {
        let (mut core, _now) = sleeping_core("a");
        core.record_failure("a", true);
        assert_eq!(core.entries()[0].device.state, DeviceState::Disconnected);
    }

    /// 单次超时（未达阈值）不改态、不停轮询——瞬态抖动不动休眠。
    #[test]
    fn single_timeout_keeps_polling() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let now = Instant::now();
        core.upsert_device(dev("a", 1));
        core.record_success("a", reading(50, false), now);
        assert_eq!(core.record_timeout("a"), TimeoutOutcome::Counting(1));
        assert_eq!(core.entries()[0].device.state, DeviceState::Online);
        assert_eq!(core.poll_decision("a", now), PollDecision::Probe);
        // 成功后连击清零。
        core.record_success("a", reading(51, false), now);
        assert_eq!(core.record_timeout("a"), TimeoutOutcome::Counting(1));
    }
}
