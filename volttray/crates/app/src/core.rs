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

/// `upsert_device` 的结果（T6）：调用方（IO 线程）据此发诊断事件、
/// 把旧端点槽位标记为别名（跳过轮询 / 不进菜单 / 不 toast）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpsertOutcome {
    /// 全新条目。
    Inserted,
    /// 同 id 原位更新。
    Updated,
    /// 物理身份匹配在册条目：在册条目已重绑到新端点（状态已迁移），
    /// 旧端点应降级为别名槽位。
    Rebound {
        /// 旧端点 id（别名化对象）。
        old_id: String,
        /// 新端点 id（在册条目现在的 id）。
        new_id: String,
    },
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
    /// 已证实的物理身份 unitId（T6；取不到时为 None——Name 降级身份
    /// 不外发，GUI 侧仅用它做 rebind 后的 pin 重绑匹配）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub physical_key: Option<String>,
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
    /// 阈值热改即时重估产生的待派发告警（v0.3.14 红蓝共识 B1 修复）：
    /// `is_low_battery` 的重估告警不再丢弃，暂存于此由 GUI 侧取走派发。
    pending_alerts: Vec<(String, Alert)>,
    snap_seq: u64,
}

impl AppCore {
    pub fn new(config: Arc<RwLock<Config>>) -> Self {
        AppCore {
            config,
            entries: Vec::new(),
            lowbats: HashMap::new(),
            pending_alerts: Vec::new(),
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

    /// 注册 / 更新设备（T6：新增物理身份 rebind-on-match）。
    ///
    /// 判定顺序：
    /// 1. id 精确匹配 → 原位更新（保留电量 / 状态；新读数丢失 proven 身份时
    ///    保留旧身份，避免身份在端点间反复横跳）。
    /// 2. 物理身份匹配（双方均 proven unitId 且相等）→ **重绑**：把在册条目
    ///    的端点 id 改为新枚举端点，迁移其全部主机侧状态（低电追踪器键、
    ///    超时连击、唤醒臂、灰显计时、待派发告警的 id 引用），不新增条目。
    ///    调用方负责把旧端点标记为别名槽位（跳过轮询 / 不进菜单 / 不 toast）。
    /// 3. 都不匹配（含身份不明的 Name 降级——保守规则：宁可分开绝不误并）
    ///    → 新增条目。
    pub fn upsert_device(&mut self, device: Device) -> UpsertOutcome {
        if let Some(e) = self.entries.iter_mut().find(|e| e.device.id == device.id) {
            let battery = e.device.battery;
            let state = e.device.state;
            // 新注册没读到 unitId（降级 Name）时保留在册的 proven 身份；
            // 新注册读到了则以新值为准（同一端点身份不应变化，变化即异常，
            // 但以最新一次证实值为准更简单可观测）。
            let keep_key = if device.physical_key.is_proven() {
                None
            } else {
                Some(e.device.physical_key.clone())
            };
            e.device = device;
            e.device.battery = battery;
            e.device.state = state;
            if let Some(k) = keep_key {
                e.device.physical_key = k;
            }
            return UpsertOutcome::Updated;
        }
        if device.physical_key.is_proven() {
            if let Some(pos) = self
                .entries
                .iter()
                .position(|e| e.device.physical_key.proven_matches(&device.physical_key))
            {
                let old_id = self.entries[pos].device.id.clone();
                let new_id = device.id.clone();
                let new_index = device.device_index;
                let new_link = device.link;
                // 复用同一迁移路径（电量 / 状态 / 计时 / 追踪器 / 告警引用）。
                self.rebind_device_id(&old_id, &new_id, new_index, new_link);
                // 带入新端点的名字与 proven 身份。
                let e = &mut self.entries[pos];
                e.device.name = device.name;
                e.device.physical_key = device.physical_key;
                return UpsertOutcome::Rebound { old_id, new_id };
            }
        }
        self.entries.push(DeviceEntry::new(device));
        UpsertOutcome::Inserted
    }

    /// 把在册条目的端点 id 重绑到新端点并迁移全部主机侧状态（T6）。
    ///
    /// 迁移清单：端点 id / device_index / link、低电追踪器键（`lowbats`）、
    /// 待派发告警的 id 引用（`pending_alerts`）；条目内的超时连击、唤醒臂、
    /// 灰显计时、电量与状态随条目原位保留。旧 id 引用若遗漏任何一处，
    /// 该状态就会随旧 id 变成孤儿（低电追踪失效 / 告警指向不存在的设备）。
    pub fn rebind_device_id(
        &mut self,
        old_id: &str,
        new_id: &str,
        new_index: u8,
        new_link: volttray_device::LinkKind,
    ) -> bool {
        let Some(e) = self.entries.iter_mut().find(|e| e.device.id == old_id) else {
            return false;
        };
        e.device.id = new_id.to_string();
        e.device.device_index = new_index;
        e.device.link = new_link;
        if let Some(tracker) = self.lowbats.remove(old_id) {
            self.lowbats.insert(new_id.to_string(), tracker);
        }
        for (id, _) in self.pending_alerts.iter_mut() {
            if id == old_id {
                *id = new_id.to_string();
            }
        }
        true
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
        // v0.3.14（蓝队 B2 必修）：进程内该设备的**首次**成功读数（重启 /
        // 开机自启后的注册探测）立即按当前阈值即判（evaluate_now），不再走
        // 2 连击去抖——旧逻辑下重启后低电提醒态要等第 2 次读数（默认轮询
        // 1800s，最长约 30 分钟无闪无通知），且第 2 次读数还会把本低电期的
        // toast 重发一次。纯主机侧状态判定，零射频。取舍（已裁决）：首读数
        // 放弃去抖，极端情况下一次异常首读数可能立即告警一次；tracker 一经
        // 创建，后续读数恢复 M11 去抖语义。
        let is_first_reading = !self.lowbats.contains_key(id);
        let tracker = self
            .lowbats
            .entry(id.to_string())
            .or_insert_with(|| LowBatteryTracker::new(cfg.low_threshold));
        // 阈值热更新（设置窗口 / TOML 改动均经共享配置生效）。
        tracker.set_threshold(cfg.low_threshold);
        if is_first_reading {
            tracker.evaluate_now(reading.percent, reading.charging)
        } else {
            tracker.update(reading.percent, reading.charging)
        }
    }

    /// 指定设备当前是否处于低电触发状态（M11 托盘闪烁判定用）。
    ///
    /// v0.3.14（真机反馈「调阈值不生效」修复）：每次调用先对账配置阈值——
    /// 阈值被设置窗口 / TOML 热改后，立即用最后一次已知读数重估低电态
    /// （`LowBatteryTracker::evaluate_now`），不再等下一次轮询读数
    /// （默认 1800s 一轮，旧逻辑下阈值改动最长约 1 小时才体现）。
    /// 纯主机侧状态重估，零射频，不违反休眠设备零主动查询红线。
    /// 注意取 `&mut self`：阈值对账会更新 tracker。
    pub fn is_low_battery(&mut self, id: &str) -> bool {
        let cfg_threshold = self.config().low_threshold;
        let last_reading = self
            .entries
            .iter()
            .find(|e| e.device.id == id)
            .and_then(|e| e.device.battery);
        let Some(tracker) = self.lowbats.get_mut(id) else {
            return false;
        };
        let mut fired = None;
        if tracker.threshold() != cfg_threshold {
            tracker.set_threshold(cfg_threshold);
            if let Some(b) = last_reading {
                // v0.3.14（红蓝共识 B1 必修）：重估告警不再丢弃——暂存
                // pending_alerts，由 GUI 侧经 `take_pending_alerts` 取走、
                // 走与轮询路径相同的 toast 派发。去重由 tracker 状态保证：
                // evaluate_now 仅在「未激活 → 激活」沿返回告警，同一低电期
                // 不会重复入队，也不会与 record_success 路径重复派发。
                fired = tracker.evaluate_now(b.percent, b.charging);
            }
        }
        let active = tracker.is_active();
        if let Some(alert) = fired {
            self.pending_alerts.push((id.to_string(), alert));
        }
        active
    }

    /// 取走阈值热改即时重估产生的待派发告警（v0.3.14，B1）。
    /// 调用方（GUI pump）负责走既有 toast 派发路径（含通知总开关 /
    /// 静音配置与派发日志）。
    pub fn take_pending_alerts(&mut self) -> Vec<(String, Alert)> {
        std::mem::take(&mut self.pending_alerts)
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

    /// v0.3.15（P1-1）：唤醒探测轮内的槽位准入判定——仅「已注册且当前
    /// 休眠」的设备允许被探测。在线/灰显设备在探测轮一律跳过（唤醒信号
    /// ≠ 该查在线设备），与 `poll_decision` 组合保证：探测轮里非候选
    /// 设备一次 HID++ 查询都不发。
    pub fn wake_probe_candidate(&self, id: &str) -> bool {
        self.entries
            .iter()
            .any(|e| e.device.id == id && e.device.state == DeviceState::Sleeping)
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
                    physical_key: match &e.device.physical_key {
                        volttray_device::PhysicalKey::UnitId(u) => Some(u.clone()),
                        volttray_device::PhysicalKey::Name(_) => None,
                    },
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

    /// v0.3.15（P1-1）：唤醒探测轮只准入休眠设备——在线、未注册 id
    /// 一律排除；超时连击进入休眠后才成为候选（与 poll_decision 的
    /// arm+冷却门控串联：候选资格 ≠ 一定探测，仅允许进入判定链）。
    #[test]
    fn wake_probe_candidate_admits_only_sleeping() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        core.upsert_device(dev("a", 1));
        core.upsert_device(dev("b", 2));
        // 在线设备与未注册 id：非候选。
        assert!(!core.wake_probe_candidate("a"));
        assert!(!core.wake_probe_candidate("ghost"));
        // b 连续超时达阈值 → 进入休眠 → 成为唯一候选。
        let _ = core.record_timeout("b");
        assert!(!core.wake_probe_candidate("b"), "未达休眠阈值前仍在线");
        let _ = core.record_timeout("b");
        assert!(core.wake_probe_candidate("b"));
        assert!(!core.wake_probe_candidate("a"), "在线设备不得成为候选");
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
        // 默认档跟随 D-66：DEFAULT_POLL_SECS = 1800s（引用常量防再过时）。
        assert_eq!(
            core.poll_interval(),
            Duration::from_secs(crate::config::DEFAULT_POLL_SECS)
        );
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
        // v0.3.14（B2）：首读数即按阈值即判——19 ≤ 20 立即告警并进入低电。
        assert_eq!(
            core.record_success("a", reading(19, false), now),
            Some(Alert::LowBattery(19))
        );
        // 同一低电期内后续读数不再重复告警（去重）。
        assert_eq!(core.record_success("a", reading(18, false), now), None);
        assert!(core.is_low_battery("a"));
    }

    /// v0.3.14（蓝队 B2 必修）：重启/自启后低电提醒态不再丢失——进程内
    /// 首次成功读数（注册探测）立即按当前阈值即判，消除默认 1800s 轮询档
    /// 下最长约 30 分钟的「无闪无通知」盲区；且 toast 只发一次（旧逻辑
    /// 第 2 次读数会重发）。非首读数恢复 M11 去抖（首读高于阈值后，
    /// 仍需连续 2 次越限才触发）。
    #[test]
    fn first_reading_after_restart_evaluates_immediately() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let now = Instant::now();
        core.upsert_device(dev("a", 1));
        // 首读数低于阈值 → 立即告警（不等下一个轮询周期）。
        assert_eq!(
            core.record_success("a", reading(15, false), now),
            Some(Alert::LowBattery(15)),
            "重启后首读数必须立即恢复低电提醒"
        );
        // 首读数高于阈值 → 不触发；之后恢复 2 连击去抖。
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        core.upsert_device(dev("b", 1));
        assert_eq!(core.record_success("b", reading(30, false), now), None);
        assert_eq!(core.record_success("b", reading(19, false), now), None);
        assert_eq!(
            core.record_success("b", reading(19, false), now),
            Some(Alert::LowBattery(19)),
            "首读数之后恢复 M11 去抖语义"
        );
    }

    /// v0.3.14（红蓝共识 B1 必修）：阈值热改触发的即时重估告警进入
    /// pending 队列等待派发（此前被丢弃，本次低电期 toast 永久丢失）；
    /// 取走后清空，同一低电期不重复入队。
    #[test]
    fn threshold_hot_change_alert_queued_for_dispatch() {
        let cfg = Arc::new(RwLock::new(Config::default()));
        let mut core = AppCore::new(cfg.clone());
        let now = Instant::now();
        core.upsert_device(dev("a", 1));
        core.record_success("a", reading(30, false), now); // 30 > 20
        assert!(core.take_pending_alerts().is_empty());
        // 阈值调到电量之上 → 重估告警入队，且只入队一次。
        core.swap_config(Config {
            low_threshold: 35,
            ..Config::default()
        });
        assert!(core.is_low_battery("a"));
        let pending = core.take_pending_alerts();
        assert_eq!(
            pending,
            vec![("a".to_string(), Alert::LowBattery(30))],
            "阈值热改告警必须留给 GUI 派发"
        );
        assert!(core.take_pending_alerts().is_empty(), "取走后清空");
        assert!(core.is_low_battery("a"));
        assert!(
            core.take_pending_alerts().is_empty(),
            "同一低电期不得重复入队"
        );
        // 阈值调回 → 立即解除；再次调高 → 新的低电期重新告警一次。
        core.swap_config(Config {
            low_threshold: 20,
            ..Config::default()
        });
        assert!(!core.is_low_battery("a"));
        core.swap_config(Config {
            low_threshold: 35,
            ..Config::default()
        });
        assert!(core.is_low_battery("a"));
        assert_eq!(core.take_pending_alerts().len(), 1, "新低电期重新告警");
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

    /// v0.3.14（真机反馈「调阈值不生效」）：阈值热改后，`is_low_battery`
    /// 立即按最后一次已知读数重估——抬高立即进入低电、降低立即解除，
    /// 全程不再等待任何新的轮询读数。
    #[test]
    fn threshold_change_takes_effect_immediately_via_is_low_battery() {
        let cfg = Arc::new(RwLock::new(Config::default()));
        let mut core = AppCore::new(cfg.clone());
        let now = Instant::now();
        core.upsert_device(dev("a", 1));
        core.record_success("a", reading(30, false), now);
        assert!(!core.is_low_battery("a")); // 30 > 默认阈值 20
                                            // 阈值调到电量之上 → 立即低电（不等下一个轮询周期）。
        core.swap_config(Config {
            low_threshold: 35,
            ..Config::default()
        });
        assert!(
            core.is_low_battery("a"),
            "阈值调高必须立即按最后已知读数进入低电"
        );
        // 阈值调到电量之下 → 立即解除。
        core.swap_config(Config {
            low_threshold: 25,
            ..Config::default()
        });
        assert!(!core.is_low_battery("a"), "阈值调低必须立即解除低电");
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
        // Connected → 成功 → Online（Device::new 默认 Connected，dev() 预设 Online）
        core.upsert_device(Device::new("a".into(), "Mouse a", LinkKind::Bolt, 1, false));
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

    // ---- T6：物理身份 rebind / 保守合并 / 降级 ----

    fn dev_with_unit(id: &str, index: u8, unit: &str) -> Device {
        let mut d = dev(id, index);
        d.set_unit_id(unit);
        d
    }

    /// 验收 1 核心：同一 unitId 的新端点注册 → rebind 而非新增，
    /// 低电追踪器、电量、超时连击全部随条目迁移。
    #[test]
    fn upsert_rebinds_on_matching_unit_id_and_migrates_state() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let now = Instant::now();
        // 接收器槽位 1 注册 + 低电读数（建 tracker）+ 一次超时连击。
        core.upsert_device(dev_with_unit("C52B|usb#1", 1, "39414243"));
        core.record_success("C52B|usb#1", reading(10, false), now);
        assert_eq!(
            core.record_timeout("C52B|usb#1"),
            TimeoutOutcome::Counting(1)
        );
        assert!(core.is_low_battery("C52B|usb#1"), "低电追踪应已激活");
        // 有线模式新端点（同 unitId）注册 → Rebound。
        let out = core.upsert_device(dev_with_unit("C52B|usb&MI_02#255", 0xFF, "39414243"));
        assert_eq!(
            out,
            UpsertOutcome::Rebound {
                old_id: "C52B|usb#1".to_string(),
                new_id: "C52B|usb&MI_02#255".to_string(),
            }
        );
        // 仍只有 1 个条目，端点已重绑。
        assert_eq!(core.entries().len(), 1);
        let e = &core.entries()[0];
        assert_eq!(e.device.id, "C52B|usb&MI_02#255");
        assert_eq!(e.device.device_index, 0xFF);
        assert_eq!(e.device.battery.map(|b| b.percent), Some(10), "电量保留");
        assert_eq!(e.timeout_streak, 1, "超时连击保留");
        // 低电追踪器键已迁移：新 id 命中、旧 id 不再命中。
        assert!(core.is_low_battery("C52B|usb&MI_02#255"));
        assert!(!core.lowbats.contains_key("C52B|usb#1"));
    }

    /// rebind 后 pending_alerts 的 id 引用一并改写（不指向孤儿 id）。
    #[test]
    fn rebind_rewrites_pending_alert_ids() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let now = Instant::now();
        core.upsert_device(dev_with_unit("old", 1, "AAAA0001"));
        core.record_success("old", reading(50, false), now);
        // 造一条 pending alert：热改阈值触发重估入队。
        core.swap_config(Config {
            low_threshold: 90,
            ..Config::default()
        });
        assert!(core.is_low_battery("old"));
        assert_eq!(core.take_pending_alerts().len(), 1);
        // 再来一条入队后不取走，rebind 应改写其 id。
        core.swap_config(Config {
            low_threshold: 20,
            ..Config::default()
        });
        assert!(!core.is_low_battery("old"));
        let _ = core.upsert_device(dev_with_unit("new", 0xFF, "AAAA0001"));
        let alerts = core.take_pending_alerts();
        assert!(alerts.iter().all(|(id, _)| id == "new"));
    }

    /// 验收 2：不同 unitId 永不合并——两台独立物理鼠标始终 2 条目。
    #[test]
    fn distinct_unit_ids_stay_separate() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        core.upsert_device(dev_with_unit("C52B|usb#1", 1, "AAAA0001")); // PRO X 2
        let out = core.upsert_device(dev_with_unit("C52B|usb#2", 2, "BBBB0002")); // G304
        assert_eq!(out, UpsertOutcome::Inserted);
        assert_eq!(core.entries().len(), 2);
    }

    /// 验收 3：身份不明（Name 降级）不合并——与现状行为一致。
    #[test]
    fn unproven_identity_never_merges() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        // 同名两台（Name 降级）：不合并。
        core.upsert_device(dev("a", 1));
        assert_eq!(core.upsert_device(dev("b", 2)), UpsertOutcome::Inserted);
        assert_eq!(core.entries().len(), 2);
        // 一侧 proven、一侧不明：不合并。
        assert_eq!(
            core.upsert_device(dev_with_unit("c", 3, "AAAA0001")),
            UpsertOutcome::Inserted
        );
        assert_eq!(core.entries().len(), 3);
        // 新注册 proven 但在册不明：同样不合并。
        let mut core2 = AppCore::new(Arc::new(RwLock::new(Config::default())));
        core2.upsert_device(dev("x", 1));
        assert_eq!(
            core2.upsert_device(dev_with_unit("y", 0xFF, "AAAA0001")),
            UpsertOutcome::Inserted
        );
        assert_eq!(core2.entries().len(), 2);
    }

    /// 同 id 更新：新注册未读到 unitId 时保留在册 proven 身份。
    #[test]
    fn update_preserves_proven_key_when_reread_fails() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        core.upsert_device(dev_with_unit("a", 1, "AAAA0001"));
        // 同端点重注册，但这次没读到 unitId（Name 降级）。
        assert_eq!(core.upsert_device(dev("a", 1)), UpsertOutcome::Updated);
        assert_eq!(
            core.entries()[0].device.physical_key,
            volttray_device::PhysicalKey::UnitId("AAAA0001".into())
        );
        // 同端点重注册读到了 unitId：以新值为准。
        assert_eq!(
            core.upsert_device(dev_with_unit("a", 1, "AAAA0009")),
            UpsertOutcome::Updated
        );
        assert_eq!(
            core.entries()[0].device.physical_key,
            volttray_device::PhysicalKey::UnitId("AAAA0009".into())
        );
    }

    /// 快照透出 proven 物理身份（GUI pin 重绑匹配用）；降级身份不外发。
    #[test]
    fn snapshot_exposes_only_proven_physical_key() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let now = Instant::now();
        core.upsert_device(dev_with_unit("a", 1, "AAAA0001"));
        core.upsert_device(dev("b", 2));
        let snap = core.snapshot(now);
        assert_eq!(snap.devices[0].physical_key.as_deref(), Some("AAAA0001"));
        assert_eq!(snap.devices[1].physical_key, None);
    }

    /// rebind_device_id：端点拔线后向存活别名晋级时复用的迁移路径。
    #[test]
    fn rebind_device_id_migrates_tracker_and_returns_false_for_unknown() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let now = Instant::now();
        core.upsert_device(dev_with_unit("a", 0xFF, "AAAA0001"));
        core.record_success("a", reading(5, false), now);
        assert!(core.rebind_device_id("a", "b", 1, LinkKind::Bolt));
        assert_eq!(core.entries()[0].device.id, "b");
        assert_eq!(core.entries()[0].device.device_index, 1);
        assert!(core.lowbats.contains_key("b"));
        assert!(!core.lowbats.contains_key("a"));
        assert!(!core.rebind_device_id("ghost", "c", 1, LinkKind::Bolt));
    }
}
