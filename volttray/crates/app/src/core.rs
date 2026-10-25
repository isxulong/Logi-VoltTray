//! 应用核心：设备台账、快照、默认展示设备、90s 灰显派生、调度节奏。
//!
//! 与传输层解耦：本模块只维护状态与纯逻辑决策，全部桩上可测。

use crate::config::Config;
use crate::debounce::{Alert, LowBatteryTracker};
use crate::persist::{LastKnownStore, PersistedDevice};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use volttray_device::battery::{BatteryReading, BatterySource};
use volttray_device::state::{mark_online, DeviceState};
use volttray_device::{Device, LinkKind, PhysicalKey};

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
/// 灰显阈值锚定倍数（F5 主修复 (a)，v0.3.16 线）：灰显阈值 =
/// `max(GRAY_AFTER, GRAY_MIN_POLLS × 本设备轮询档)`（WP-C 起按设备
/// 档位锚定，见 `AppCore::gray_threshold_for`）。纯墙钟 90s 派生在
/// 轮询档 >90s（档位 300~7200s，D-66）时必然误灰——两次连续轮询失败
/// 才有「无响应」证据，阈值至少覆盖两个完整轮询周期。仅作兜底：鼠标
/// 活跃期的新鲜读数主要由临近到期豁免（NEAR_GRAY_LEEWAY）保证。
pub const GRAY_MIN_POLLS: u32 = 2;
/// 临近到期豁免提前量（F5 主修复 (b)，v0.3.16 线；WP-C 起锚从「90s
/// 墙钟」迁到「本设备自己的 next_due」）：在线、非充电设备的
/// `next_due - now ≤ 本窗口` 时，探测轮对其提前放行一次查询——探测轮
/// 与随后的到期唤醒合并成一次查询，last_success 按本设备档位刷新。
/// 充电档（15s）不吃提前放行：30s 提前量会破坏 AC-C1 的 15s±2s 节拍
/// （同 D-72 原豁免 ≥60s 窗口永不触及 15s 档的取舍）。
pub const NEAR_GRAY_LEEWAY: Duration = Duration::from_secs(30);

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
    /// A-1b 注册归并：新注册设备按 physical_key 字符串合并了一个虚拟
    /// 条目（持久化注入）。条目已重绑到真实端点、virtual 标记已清除，
    /// 持久化最后已知电量保留到首个实时读数接管。
    MergedVirtual {
        /// 虚拟条目 id（`virtual#{key}`，调用方清理引用）。
        old_id: String,
        /// 真实端点 id（条目现在的 id）。
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
    /// A-1b 虚拟条目标记：由持久化记录启动注入、非真实注册设备。
    /// 零查询、不参与调度（各处 is_virtual 守卫保证）。
    pub is_virtual: bool,
    /// 持久化记录的 last_seen（epoch 秒；虚拟条目时间标注用，
    /// 注册归并后清除）。
    pub persisted_last_seen: Option<u64>,
    /// WP-C（v3 C-1）：本设备下一次查询到期点（按设备计时轮询）。
    /// None = 不排程——休眠（D-33 零主动查询）、虚拟条目、或已被
    /// 注销（gone）等重枚举；`is_due` 对「在线但从未排程」兜底为
    /// 立即到期（新注册首读 / 状态残留自愈）。
    pub next_due: Option<Instant>,
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
            is_virtual: false,
            persisted_last_seen: None,
            next_due: None,
        }
    }

    /// WP-C（v3 C-1）：本设备当前轮询档——Charging 15s /
    /// Normal 配置档；休眠与虚拟条目不排程（None）。Disconnected
    /// 归 Normal 档（重枚举前的兜底节奏，实际槽位已随通道移除）。
    /// 相位二（第 4 项）：experimental_per_device_polling=false 时充电
    /// 设备不再走 15s 快档、统一按配置档——每设备分档轮询本身是实验
    /// 特性，关闭即退回单档节奏；D-33 休眠零查询不受影响（上面的
    /// Sleeping → None 分支恒保持，与实验开关无关）。
    pub fn tier_interval(&self, cfg: &Config) -> Option<Duration> {
        if self.is_virtual || self.device.state == DeviceState::Sleeping {
            return None;
        }
        if cfg.experimental_per_device_polling && self.device.battery.is_some_and(|b| b.charging) {
            Some(POLL_CHARGING)
        } else {
            Some(Duration::from_secs(cfg.poll_interval_secs))
        }
    }

    /// 派生灰显态（超过阈值无成功；从未成功过的在线设备不灰）。
    /// 阈值由调用方经 `AppCore::gray_threshold_for()` 按本设备档位
    /// 给出（F5 主修复 (a)：锚定轮询档，不再是固定 90s；WP-C 起按
    /// 设备各自档位）。
    pub fn is_gray(&self, now: Instant, threshold: Duration) -> bool {
        match self.last_success {
            Some(t) => now.duration_since(t) >= threshold,
            None => false,
        }
    }

    /// 展示用灰显（D-33）：派生灰，或休眠中——休眠显示最后一次
    /// 已知电量并灰显，不显示成「未读取 / 未知」。
    pub fn display_gray(&self, now: Instant, threshold: Duration) -> bool {
        self.device.state == DeviceState::Sleeping || self.is_gray(now, threshold)
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
    /// A-1b 虚拟条目标记（持久化注入、非真实注册设备）。
    pub is_virtual: bool,
    /// 持久化记录的最后已知时刻（epoch 秒；虚拟条目时间标注用）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<u64>,
}

impl DeviceSnapshot {
    /// A-2 菜单行使能判定（v3）：有实时或持久化最后已知电量即可点
    /// ——真实条目 ∪ 虚拟条目统一判定，「条目存在」不再以注册成功
    /// 为前提。离线 gray 行同样可点（点击仅钉选 + refresh_ui，
    /// 零查询）。
    pub fn has_displayable_data(&self) -> bool {
        self.percent.is_some()
    }
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
    /// 回退键冲突集合（v3 R5-I-01）：≥2 个真实条目共享同一 Name
    /// 回退键即冲突——冲突键不入持久化、不生成虚拟条目、不展示
    /// 持久化最后已知。由 `refresh_key_conflicts` 维护。
    conflicted_keys: HashSet<String>,
    snap_seq: u64,
}

impl AppCore {
    pub fn new(config: Arc<RwLock<Config>>) -> Self {
        AppCore {
            config,
            entries: Vec::new(),
            lowbats: HashMap::new(),
            pending_alerts: Vec::new(),
            conflicted_keys: HashSet::new(),
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

    /// D-74（相位二第 5 项）：配置换入后批量重排到期点——轮询档位/实验
    /// 开关变更即时生效，不再等各设备自然到期才按新节奏走。规则：
    /// - 虚拟条目与休眠设备恒 None（不排程——D-33 休眠零查询红线不受
    ///   重排影响，与 tier_interval 同口径）；
    /// - 在线设备一律 now + 新档间隔（换入瞬间全体对齐到新节奏，不保留
    ///   剩余时间——语义简单、无饿死风险；最长等待 = 新档一个周期）。
    ///
    /// 调用点：配置热重载（watcher.poll() → swap_config 之后）。设置
    /// 窗口改动经 TOML 落盘 → mtime 热重载进入同一路径，无需单独通知。
    pub fn retier_all(&mut self, now: Instant) {
        let cfg = self.config();
        for e in &mut self.entries {
            e.next_due = e.tier_interval(&cfg).map(|d| now + d);
        }
    }

    /// 注册 / 更新设备（T6：物理身份 rebind-on-match；A-1b：虚拟条目归并）。
    ///
    /// 判定顺序：
    /// 1. id 精确匹配 → 原位更新（保留电量 / 状态；新读数丢失 proven 身份时
    ///    保留旧身份，避免身份在端点间反复横跳）。
    /// 2. A-1b 虚拟条目归并：在册虚拟条目的 physical_key 字符串与新设备
    ///    相等（持久化 schema 不记 provenance，合并一律按字符串相等）→
    ///    条目重绑到真实端点、virtual 标记清除、状态迁 Online；持久化
    ///    最后已知电量保留到首个实时读数接管（注册即「?」是 A-1b 要
    ///    消灭的场景）。
    /// 3. 物理身份匹配（双方均 proven unitId 且相等）→ **重绑**：把在册条目
    ///    的端点 id 改为新枚举端点，迁移其全部主机侧状态（低电追踪器键、
    ///    超时连击、唤醒臂、灰显计时、待派发告警的 id 引用），不新增条目。
    ///    调用方负责把旧端点标记为别名槽位（跳过轮询 / 不进菜单 / 不 toast）。
    /// 4. 都不匹配（含身份不明的 Name 降级——保守规则：宁可分开绝不误并）
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
        // A-1b 虚拟条目归并（判定顺序第 2 条，见上）。
        if let Some(pos) = self.entries.iter().position(|e| {
            e.is_virtual && e.device.physical_key.as_str() == device.physical_key.as_str()
        }) {
            let old_id = self.entries[pos].device.id.clone();
            let new_id = device.id.clone();
            let new_index = device.device_index;
            let new_link = device.link;
            // 持久化最后已知电量：实时读数接管前保留展示（新注册设备
            // 一般不带读数）。
            let kept_battery = self.entries[pos].device.battery;
            // 复用同一迁移路径（低电追踪器键 / 待派发告警引用）。
            self.rebind_device_id(&old_id, &new_id, new_index, new_link);
            let e = &mut self.entries[pos];
            e.is_virtual = false;
            e.persisted_last_seen = None;
            e.device.name = device.name;
            e.device.physical_key = device.physical_key;
            e.device.is_hidpp1 = device.is_hidpp1;
            e.device.battery = device.battery.or(kept_battery);
            // 设备已醒来并完成注册握手（Sleeping → Online 合法迁移）。
            e.device.state = mark_online(e.device.state);
            return UpsertOutcome::MergedVirtual { old_id, new_id };
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

    /// 注册前离线判定（小修批第 2 项）：`upsert_device` 之前调用，回答
    /// 「这台设备对应的在册条目当前是否处于 Sleeping / Disconnected」。
    ///
    /// 必须在 upsert 前取数的原因：upsert 的 MergedVirtual 分支会把虚拟
    /// 条目状态直接迁移为 Online（`mark_online`），事后再读在册状态恒为
    /// 在线——v0.3.23 真机反证：两次 no-device 恢复（日志有
    /// `no-device: exited`）但 `wake_recovery_via_reenum` 恒 0，根因即
    /// 调用点在 upsert 之后读状态。
    ///
    /// 身份匹配口径与 `upsert_device` 三条分支一一对应：同端点 id
    /// （Updated）／虚拟条目同物理键（MergedVirtual）／proven 物理键相等
    /// （Rebound）；全新插入（Inserted）无在册条目，返回 false。
    pub fn pre_registration_offline(&self, device: &Device) -> bool {
        self.entries.iter().any(|e| {
            let same_identity = e.device.id == device.id
                || (e.is_virtual && e.device.physical_key.as_str() == device.physical_key.as_str())
                || (device.physical_key.is_proven()
                    && e.device.physical_key.proven_matches(&device.physical_key));
            same_identity
                && matches!(
                    e.device.state,
                    DeviceState::Sleeping | DeviceState::Disconnected
                )
        })
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

    /// A-1b 启动注入：把一条持久化记录播种为虚拟条目。返回是否播种。
    ///
    /// 不播种（返回 false）的情形：同键已有条目（真实或虚拟——真实
    /// 条目的实时链路优先，虚拟不重复播种）；键在冲突集合内
    /// （R5-I-01：冲突设备不生成虚拟条目）。
    ///
    /// 虚拟条目语义：id 取 `virtual#{key}`（不会与真实端点 id 冲突）；
    /// 状态直接赋 Sleeping（Connected→Sleeping 在迁移表中是非法迁移，
    /// 虚拟条目不走路径迁移、纯展示语义）；physical_key 一律取
    /// Name(key)（持久化不记 provenance，归并按字符串相等）；电量即
    /// 持久化最后已知；last_success 保持 None。零查询、不参与调度——
    /// `poll_decision` / 唤醒探测 / 唤醒装填对其一律 Skip（各处
    /// is_virtual 守卫），`next_due` 恒 None 由调用方（IO 循环不为
    /// 虚拟 id 排程）保证。
    pub fn seed_virtual(&mut self, rec: &PersistedDevice) -> bool {
        if rec.physical_key.is_empty()
            || self.conflicted_keys.contains(&rec.physical_key)
            || self
                .entries
                .iter()
                .any(|e| e.device.physical_key.as_str() == rec.physical_key)
        {
            return false;
        }
        let mut device = Device::new(
            format!("virtual#{}", rec.physical_key),
            &rec.display_name,
            LinkKind::Unknown,
            0,
            false,
        );
        device.physical_key = PhysicalKey::Name(rec.physical_key.clone());
        device.battery = Some(BatteryReading {
            percent: rec.percent,
            charging: rec.charging,
            // source 字符串无法还原时回退占位（仅影响诊断展示，不影响
            // 电量数值与归并）。
            source: BatterySource::from_name(&rec.source).unwrap_or(BatterySource::UnifiedBattery),
        });
        device.state = DeviceState::Sleeping;
        let mut entry = DeviceEntry::new(device);
        entry.is_virtual = true;
        entry.persisted_last_seen = Some(rec.last_seen);
        self.entries.push(entry);
        true
    }

    /// 重算回退键冲突集合（v3 R5-I-01 键冲突条款）：≥2 个**真实**
    /// 条目共享同一 Name 回退键（同名同型号、unitId 均取不到）即
    /// 冲突。虚拟条目不计数（虚拟 + 真实同键是待归并，不是冲突）；
    /// proven unitId 不参与（同 unitId 双条目已被 rebind 路径合并）。
    ///
    /// 返回本次新进入冲突的键——调用方据此：记日志一次 + 持久化
    /// `set_conflicted`（停写该键并弃用存量记录）+ 清同键虚拟条目。
    /// 设备拔出后集合收缩（冲突解除、恢复可写），收缩不产生返回项。
    pub fn refresh_key_conflicts(&mut self) -> Vec<String> {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for e in &self.entries {
            if !e.is_virtual {
                if let PhysicalKey::Name(name) = &e.device.physical_key {
                    *counts.entry(name.as_str()).or_insert(0) += 1;
                }
            }
        }
        let next: HashSet<String> = counts
            .into_iter()
            .filter(|(_, n)| *n >= 2)
            .map(|(k, _)| k.to_string())
            .collect();
        let mut newly: Vec<String> = next.difference(&self.conflicted_keys).cloned().collect();
        newly.sort();
        self.conflicted_keys = next;
        newly
    }

    /// 当前回退键冲突集合（持久化同步 / 诊断用）。
    pub fn conflicted_keys(&self) -> &HashSet<String> {
        &self.conflicted_keys
    }

    /// 清除指定键的虚拟条目（持久化记录被 LRU/字节上限淘汰、或冲突
    /// 弃用时同步清除——A-1b「清除」段）。只动虚拟条目，真实条目不
    /// 受影响（冲突设备的实时显示不受持久化弃用影响）。返回被清除
    /// 的条目 id。
    pub fn remove_virtual_by_keys(&mut self, keys: &[String]) -> Vec<String> {
        let mut removed = Vec::new();
        self.entries.retain(|e| {
            let drop = e.is_virtual && keys.iter().any(|k| k == e.device.physical_key.as_str());
            if drop {
                removed.push(e.device.id.clone());
            }
            !drop
        });
        for id in &removed {
            self.lowbats.remove(id);
        }
        removed
    }

    /// 持久化采集钩子（A-1 变化驱动的唯一收敛点）：成功读数后在
    /// `record_success` 之外由 IO 循环调用。虚拟条目不产生持久化
    /// 写入（虚拟条目没有真实读数，其数据本就来自持久化）；冲突键
    /// 由 store 内部拦截。返回是否新标 dirty。
    pub fn persist_note_success(
        &self,
        store: &mut LastKnownStore,
        id: &str,
        wall_secs: u64,
        now: Instant,
    ) -> bool {
        let Some(e) = self.entries.iter().find(|e| e.device.id == id) else {
            return false;
        };
        if e.is_virtual {
            return false;
        }
        let Some(b) = e.device.battery else {
            return false;
        };
        store.note_reading(
            e.device.physical_key.as_str(),
            &e.device.name,
            b.percent,
            b.charging,
            b.source.name(),
            wall_secs,
            now,
        )
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
        let cfg = self.config();
        let entry = self.entries.iter_mut().find(|e| e.device.id == id)?;
        entry.last_success = Some(now);
        entry.gray_alerted = false;
        entry.timeout_streak = 0;
        entry.wake_armed = false;
        entry.last_wake_probe = None;
        entry.device.state = mark_online(entry.device.state);
        entry.device.battery = Some(reading);
        // WP-C（v3 C-1/C-3）：充/拔切换随本次读数立即重排本设备
        // 到期点——充电 15s（AC-C1）/ 非充电配置档（AC-C2：拔线感知
        // 延迟 ≤ 一个充电档 15s，无需拔线事件监听）。
        entry.next_due = entry.tier_interval(&cfg).map(|iv| now + iv);
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
    /// WP-C：进休眠 / 探测失败 → next_due 置 None（等唤醒信号重排）；
    /// 未达阈值仍在计数 → 按本设备档位重排下一次到期。
    pub fn record_timeout(&mut self, id: &str, now: Instant) -> TimeoutOutcome {
        let cfg = self.config();
        let Some(entry) = self.entries.iter_mut().find(|e| e.device.id == id) else {
            return TimeoutOutcome::Counting(0);
        };
        entry.timeout_streak = entry.timeout_streak.saturating_add(1);
        match entry.device.state {
            DeviceState::Sleeping => {
                entry.next_due = None;
                TimeoutOutcome::AlreadySleeping
            }
            DeviceState::Online if entry.timeout_streak >= SLEEP_AFTER_TIMEOUTS => {
                entry.device.set_state(DeviceState::Sleeping);
                entry.wake_armed = false;
                entry.last_wake_probe = None;
                entry.next_due = None;
                TimeoutOutcome::EnteredSleep
            }
            // Connected / Disconnected 下只计数、不改态
            //（Disconnected 由 gone 路径管理，GrayedOut 不落库）。
            _ => {
                entry.next_due = entry.tier_interval(&cfg).map(|iv| now + iv);
                TimeoutOutcome::Counting(entry.timeout_streak)
            }
        }
    }

    /// 被动唤醒信号（D-33）：系统层观测到鼠标活动（Raw Input）→ 给休眠
    /// 设备装填一次唤醒探测资格。
    /// R-3（v0.3.17）：装填范围扩展为 Sleeping ∪ (Online ∧ 灰显)——
    /// 灰显在线设备电量未知，鼠标已活动时装填一次探测资格，让它随
    /// 唤醒探测轮提前刷新（不装填就要等到档位到期才自愈）。新鲜在线
    /// 设备忽略（正常轮询节奏不变）。
    /// A-1b：虚拟条目不装填（无真实端点可探测）。
    /// WP-C（v3 C-1，冷却 per-entry 不变 I-12）：装填同时把本设备
    /// next_due 提到「尽快」——冷却窗口内排在冷却结束点（探测节奏仍
    /// 受 WAKE_PROBE_COOLDOWN 约束），否则立即到期。
    pub fn note_wake_signal(&mut self, id: &str, now: Instant) {
        let cfg = self.config();
        if let Some(entry) = self.entries.iter_mut().find(|e| e.device.id == id) {
            let armable = !entry.is_virtual
                && (entry.device.state == DeviceState::Sleeping
                    || (entry.device.state == DeviceState::Online
                        && entry.is_gray(now, gray_threshold_of(entry, &cfg))));
            if armable {
                entry.wake_armed = true;
                entry.next_due = Some(match entry.last_wake_probe {
                    Some(t) if now.duration_since(t) < WAKE_PROBE_COOLDOWN => {
                        t + WAKE_PROBE_COOLDOWN
                    }
                    _ => now,
                });
            }
        }
    }

    /// v0.3.15（P1-1）：唤醒探测轮内的槽位准入判定——仅「已注册且当前
    /// 休眠」的设备允许被探测。在线/灰显设备在探测轮一律跳过（唤醒信号
    /// ≠ 该查在线设备），与 `poll_decision` 组合保证：探测轮里非候选
    /// 设备一次 HID++ 查询都不发。A-1b：虚拟条目一律非候选。
    pub fn wake_probe_candidate(&self, id: &str) -> bool {
        self.entries
            .iter()
            .any(|e| e.device.id == id && e.device.state == DeviceState::Sleeping && !e.is_virtual)
    }

    /// R-3（v0.3.17）：已装填唤醒资格的「在线且灰显」设备本轮是否允许
    /// 被探测——与休眠候选同口径（资格已装填 + 已过 WAKE_PROBE_COOLDOWN
    /// 冷却），只是设备状态为 Online ∧ 灰显。探测轮据此把此类设备从
    /// 「一律跳过」放宽为「放行一次」。虚拟条目一律 false。
    pub fn wake_armed_gray(&self, id: &str, now: Instant) -> bool {
        let cfg = self.config();
        self.entries.iter().any(|e| {
            e.device.id == id
                && !e.is_virtual
                && e.device.state == DeviceState::Online
                && e.wake_armed
                && e.last_wake_probe
                    .is_none_or(|t| now.duration_since(t) >= WAKE_PROBE_COOLDOWN)
                && e.is_gray(now, gray_threshold_of(e, &cfg))
        })
    }

    /// 临近到期豁免（WP-C，v3 C-1/C-4：D-72 临近灰显豁免与 per-device
    /// due 的统一形态——一套机制，不再两套并存）：在线、非充电设备的
    /// 下一次到期距今 ≤ NEAR_GRAY_LEEWAY 时返回 true，探测轮据此对其
    /// 提前放行一次查询——把最多 30s 后本该做的查询提前并轮，last_success
    /// 按本设备档位持续刷新，F5 断言（正常使用中在线设备不因档位冻结
    /// 被误判灰显）随之迁移到按设备计时口径：锚不再是 90s 墙钟，而是
    /// 本设备自己的 next_due。
    /// 充电档排除（AC-C1：30s 提前量会破坏 15s±2s 节拍）；休眠设备
    /// 走 D-33 唤醒路径（`wake_probe_candidate`），与本判定按状态互斥。
    pub fn near_due_exemption(&self, id: &str, now: Instant) -> bool {
        self.entries.iter().any(|e| {
            e.device.id == id
                && e.device.state == DeviceState::Online
                && !e.is_virtual
                && !e.device.battery.is_some_and(|b| b.charging)
                && e.next_due.is_some_and(|d| {
                    d > now && d.saturating_duration_since(now) <= NEAR_GRAY_LEEWAY
                })
        })
    }

    /// 探测轮槽位准入的合并裁决点（WP-C 与 D-33）：
    /// 休眠候选（唤醒探测）、已装填唤醒资格的在线灰显设备（R-3，
    /// v0.3.17）或临近到期豁免命中者放行，其余在线设备跳过。
    pub fn probe_round_allows(&self, id: &str, now: Instant) -> bool {
        self.wake_probe_candidate(id)
            || self.wake_armed_gray(id, now)
            || self.near_due_exemption(id, now)
    }

    /// 当前处于休眠的**真实**设备 id 列表（IO 线程收到唤醒信号后批量
    /// 装填用）。A-1b：虚拟条目（Sleeping 展示态）不在其列——装填
    /// 虚拟条目意味着向不存在的端点发查询，违反零查询红线。
    pub fn sleeping_ids(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|e| e.device.state == DeviceState::Sleeping && !e.is_virtual)
            .map(|e| e.device.id.clone())
            .collect()
    }

    /// R-3（v0.3.17）：当前「在线且灰显」的**真实**设备 id 列表——
    /// 唤醒装填扩展集 (Online ∧ 灰显) 的枚举口径，与 sleeping_ids
    /// 并列构成唤醒装填全集 Sleeping ∪ (Online ∧ 灰显)。虚拟条目
    /// 不在其列（同 sleeping_ids 的零查询红线口径）。
    pub fn online_gray_ids(&self, now: Instant) -> Vec<String> {
        let cfg = self.config();
        self.entries
            .iter()
            .filter(|e| {
                !e.is_virtual
                    && e.device.state == DeviceState::Online
                    && e.is_gray(now, gray_threshold_of(e, &cfg))
            })
            .map(|e| e.device.id.clone())
            .collect()
    }

    /// 一次唤醒探测开始（D-33）：消耗唤醒资格并进入冷却——本次探测
    /// 无论成败，都要等下一个唤醒信号 + 冷却窗口过后才会再探测。
    /// WP-C：探测在途期间摘除排程（结果会经 record_success /
    /// record_timeout 重新排程）。
    pub fn begin_wake_probe(&mut self, id: &str, now: Instant) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.device.id == id) {
            entry.wake_armed = false;
            entry.last_wake_probe = Some(now);
            entry.next_due = None;
        }
    }

    /// 本轮是否向该设备发查询（D-33 零主动查询裁决点）：
    /// 非休眠一律 Probe；休眠设备仅当「唤醒资格已装填且已过冷却」才 Probe
    /// 一次，其余一律 Skip——休眠期间绝不发出主动查询。
    /// A-1b：虚拟条目一律 Skip（无真实端点，零查询红线）。
    pub fn poll_decision(&self, id: &str, now: Instant) -> PollDecision {
        let Some(entry) = self.entries.iter().find(|e| e.device.id == id) else {
            return PollDecision::Probe;
        };
        if entry.is_virtual {
            return PollDecision::Skip;
        }
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
    /// WP-C：gone 落 Disconnected 的同时摘除排程——槽位已随通道注销，
    /// 等重枚举重新注册后再由首读排程。
    pub fn record_failure(&mut self, id: &str, gone: bool) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.device.id == id) {
            if gone && entry.device.state != DeviceState::Disconnected {
                entry.timeout_streak = 0;
                entry.wake_armed = false;
                entry.last_wake_probe = None;
                entry.next_due = None;
                let to = DeviceState::Disconnected;
                entry.device.set_state(to);
            }
        }
    }

    /// WP-C（v3 C-1）：本设备此刻是否到期应查。休眠 / 虚拟条目恒 false
    /// （D-33 零查询红线的调度侧保险，与 `poll_decision` 双重防护）；
    /// 在线 / 已断开但从未排程的条目兜底为立即到期（新注册首读、
    /// 状态残留自愈——任意查询结果都会重新排程）。
    pub fn is_due(&self, id: &str, now: Instant) -> bool {
        self.entries.iter().any(|e| {
            e.device.id == id
                && !e.is_virtual
                && e.device.state != DeviceState::Sleeping
                && e.next_due.is_none_or(|d| now >= d)
        })
    }

    /// WP-C（v3 C-1）：是否存在「到期应查」的在线（非休眠、非虚拟）
    /// 设备——tray 侧「整轮 vs 仅探测轮」的裁决输入（任一在线设备
    /// 到期即强制整轮，探测轮不饿死各设备自己的节奏）。
    pub fn any_due(&self, now: Instant) -> bool {
        self.entries.iter().any(|e| {
            !e.is_virtual
                && e.device.state != DeviceState::Sleeping
                && e.next_due.is_none_or(|d| now >= d)
        })
    }

    /// WP-C（v3 C-1 修订）：全部在册真实设备里最近的到期点——睡眠
    /// 预算的设备侧输入（旧 full_poll_due 的「下次必查时刻」角色由
    /// 本最小值完全吸收，不再单独维护）。无排程设备 → None。
    pub fn next_poll_due(&self) -> Option<Instant> {
        self.entries
            .iter()
            .filter(|e| !e.is_virtual)
            .filter_map(|e| e.next_due)
            .min()
    }

    /// WP-C：一次查询已发出、结果为「非超时且非 gone 的失败」（错误帧 /
    /// 映射缺失 / 特性缺失等硬失败）时的重排钩子——按本设备当前档位
    /// 推后下次到期，防止到期点滞留形成对故障设备的紧循环查询。
    /// （成功走 `record_success`、超时走 `record_timeout`，各自排程。）
    pub fn reschedule(&mut self, id: &str, now: Instant) {
        let cfg = self.config();
        if let Some(e) = self.entries.iter_mut().find(|e| e.device.id == id) {
            if e.device.state != DeviceState::Sleeping {
                e.next_due = e.tier_interval(&cfg).map(|iv| now + iv);
            }
        }
    }

    /// 检查是否有设备新进入灰显（用于"电量显示已过期"通知）；返回新变灰的设备名。
    pub fn newly_gray(&mut self, now: Instant) -> Vec<String> {
        let cfg = self.config();
        let mut out = Vec::new();
        for e in &mut self.entries {
            let threshold = gray_threshold_of(e, &cfg);
            if e.device.state == DeviceState::Online && e.is_gray(now, threshold) && !e.gray_alerted
            {
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
    /// 不被派生灰显排除——否则唯一设备休眠后图标又回退「?」。
    /// F5 主修复第 3 件（v0.3.16 线）：Online+灰显与 Sleeping 同权——
    /// 灰显的语义是「读数过期」而非「设备离线」，保留展示资格、灰显
    /// 显示最后一次已知电量，不再因派生灰回退「?」。
    pub fn default_display_index(&self, now: Instant) -> Option<usize> {
        let cfg = self.config();
        let eligible = |e: &DeviceEntry| {
            e.device.state == DeviceState::Sleeping
                || e.device.state == DeviceState::Online
                || !e.is_gray(now, gray_threshold_of(e, &cfg))
        };
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

    /// 灰显阈值（F5 主修复 (a)，v0.3.16 线；WP-C 起按设备档位）：
    /// `max(GRAY_AFTER, GRAY_MIN_POLLS × 本设备轮询档)`——充电档 15s
    /// → 90s；Normal 档（默认 1800s）→ 3600s。兜底件——保证「两次连续
    /// 轮询失败才有灰显证据」；活跃期的新鲜读数由临近到期豁免
    /// （`near_due_exemption`）与本设备自身排程保证，真离线检测仍走
    /// 既有路径（DeviceGone 即时注销 / 超时×2→Sleeping / 硬失败×3 注销）。
    pub fn gray_threshold_for(&self, e: &DeviceEntry) -> Duration {
        gray_threshold_of(e, &self.config())
    }

    /// 生成快照。
    pub fn snapshot(&mut self, now: Instant) -> Snapshot {
        self.snap_seq += 1;
        let default_device = self.default_display_index(now);
        let cfg = self.config();
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
                    gray: e.display_gray(now, gray_threshold_of(e, &cfg)),
                    source: e.device.battery.map(|b| b.source.name().to_string()),
                    physical_key: match &e.device.physical_key {
                        volttray_device::PhysicalKey::UnitId(u) => Some(u.clone()),
                        volttray_device::PhysicalKey::Name(_) => None,
                    },
                    is_virtual: e.is_virtual,
                    last_seen: e.persisted_last_seen,
                })
                .collect(),
            default_device,
        }
    }
}

/// 灰显阈值的按设备计算（WP-C，v3 C-4）：`max(GRAY_AFTER,
/// GRAY_MIN_POLLS × 本设备轮询档)`。休眠 / 虚拟条目按 Normal 档兜底
/// （其灰显由状态派生，阈值基本不参与）。借用约束友好：只依赖
/// entry 与 cfg，可在 `entries` 可变迭代中使用。
fn gray_threshold_of(e: &DeviceEntry, cfg: &Config) -> Duration {
    let iv = e
        .tier_interval(cfg)
        .unwrap_or_else(|| Duration::from_secs(cfg.poll_interval_secs));
    GRAY_AFTER.max(iv * GRAY_MIN_POLLS)
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

    /// F5 主修复第 3 件：Online+灰显与 Sleeping 同权——唯一设备派生灰
    /// 后仍保留展示资格（灰显显示最后一次已知电量），不再回退「?」。
    /// 这是裁决推翻的旧语义（原 gray_devices_excluded_from_default）。
    #[test]
    fn gray_online_device_keeps_default_display_right() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let t0 = Instant::now();
        core.upsert_device(dev("a", 1));
        core.record_success("a", reading(5, false), t0);
        let later = t0 + core.gray_threshold_for(&core.entries()[0]) + Duration::from_secs(1);
        // 已派生灰（阈值已过）但仍在默认展示位：图标灰显 5%，不是「?」。
        assert!(core.entries()[0].is_gray(later, core.gray_threshold_for(&core.entries()[0])));
        assert_eq!(core.default_display_index(later), Some(0));
        // 快照同样灰显但保留展示位。
        let snap = core.snapshot(later);
        assert!(snap.devices[0].gray);
        assert_eq!(snap.devices[0].percent, Some(5));
        assert_eq!(snap.default_device, Some(0));
    }

    /// 同权不覆盖真离线终点：Disconnected 且已灰的设备仍被排除——
    /// 排除依据是「无成功读数 + 非活跃状态」，不是「曾灰显」。
    #[test]
    fn disconnected_gray_device_still_excluded_from_default() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let t0 = Instant::now();
        core.upsert_device(dev("a", 1));
        core.record_success("a", reading(5, false), t0);
        core.record_failure("a", true); // gone → Disconnected
        let later = t0 + core.gray_threshold_for(&core.entries()[0]) + Duration::from_secs(1);
        assert_eq!(core.default_display_index(later), None);
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
        let now = Instant::now();
        let _ = core.record_timeout("b", now);
        assert!(!core.wake_probe_candidate("b"), "未达休眠阈值前仍在线");
        let _ = core.record_timeout("b", now);
        assert!(core.wake_probe_candidate("b"));
        assert!(!core.wake_probe_candidate("a"), "在线设备不得成为候选");
    }

    /// R-3（v0.3.17）①：唤醒信号给「在线且灰显」设备装填探测资格——
    /// 装填范围 Sleeping ∪ (Online ∧ 灰显)；新鲜在线设备不装填；
    /// 装填后探测轮准入放行。
    #[test]
    fn wake_signal_arms_online_gray_device() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let t0 = Instant::now();
        core.upsert_device(dev("a", 1));
        core.upsert_device(dev("b", 2));
        core.record_success("a", reading(50, false), t0);
        core.record_success("b", reading(60, false), t0);
        let threshold = core.gray_threshold_for(&core.entries()[0]);
        let later = t0 + threshold + Duration::from_secs(1);
        // b 在灰显阈值内保持新鲜；a 已派生灰。
        core.record_success("b", reading(60, false), later - Duration::from_secs(1));
        core.note_wake_signal("a", later);
        core.note_wake_signal("b", later);
        assert!(core.wake_armed_gray("a", later), "在线灰显设备应被装填");
        assert!(!core.wake_armed_gray("b", later), "新鲜在线设备不得装填");
        assert!(core.probe_round_allows("a", later), "装填后探测轮放行");
    }

    /// R-3（v0.3.17）②：装填的灰显设备与休眠候选同受
    /// WAKE_PROBE_COOLDOWN 约束——冷却窗口内不放行，冷却过后放行。
    #[test]
    fn wake_armed_gray_respects_cooldown() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let t0 = Instant::now();
        core.upsert_device(dev("a", 1));
        core.record_success("a", reading(50, false), t0);
        let threshold = core.gray_threshold_for(&core.entries()[0]);
        let later = t0 + threshold + Duration::from_secs(1);
        core.note_wake_signal("a", later);
        assert!(core.wake_armed_gray("a", later));
        // 探测消耗资格并进入冷却；冷却内再次装填也不放行。
        core.begin_wake_probe("a", later);
        core.note_wake_signal("a", later + Duration::from_secs(1));
        assert!(
            !core.wake_armed_gray("a", later + Duration::from_secs(1)),
            "冷却窗口内不放行"
        );
        assert!(
            core.wake_armed_gray("a", later + WAKE_PROBE_COOLDOWN + Duration::from_secs(1)),
            "冷却过后放行"
        );
    }

    /// R-3（v0.3.17）③：online_gray_ids 枚举口径——在线灰显真实设备
    /// 在列；新鲜在线、休眠设备不在列（休眠走 sleeping_ids）。
    #[test]
    fn online_gray_ids_excludes_fresh_and_sleeping() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let t0 = Instant::now();
        core.upsert_device(dev("a", 1));
        core.upsert_device(dev("b", 2));
        core.upsert_device(dev("c", 3));
        core.record_success("a", reading(50, false), t0);
        core.record_success("b", reading(60, false), t0);
        core.record_success("c", reading(70, false), t0);
        let threshold = core.gray_threshold_for(&core.entries()[0]);
        let later = t0 + threshold + Duration::from_secs(1);
        // b 保持新鲜；c 超时连击进入休眠。
        core.record_success("b", reading(60, false), later - Duration::from_secs(1));
        let _ = core.record_timeout("c", later);
        let _ = core.record_timeout("c", later);
        assert_eq!(core.online_gray_ids(later), vec!["a".to_string()]);
    }

    #[test]
    fn newly_gray_fires_once_and_recovers() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let t0 = Instant::now();
        core.upsert_device(dev("a", 1));
        core.record_success("a", reading(80, false), t0);
        // F5 主修复 (a)：默认档 1800s 下阈值锚定为 3600s，91s 不再触发。
        let threshold = core.gray_threshold_for(&core.entries()[0]);
        assert_eq!(
            threshold,
            GRAY_AFTER.max(Duration::from_secs(1800) * GRAY_MIN_POLLS)
        );
        assert!(core
            .newly_gray(t0 + GRAY_AFTER + Duration::from_secs(1))
            .is_empty());
        let later = t0 + threshold + Duration::from_secs(1);
        assert_eq!(core.newly_gray(later), vec!["Mouse a".to_string()]);
        assert!(core.newly_gray(later).is_empty()); // 不重复
        core.record_success("a", reading(80, false), later);
        assert!(core.newly_gray(later).is_empty());
    }

    /// F5 主修复 (a) 阈值锚定（WP-C 迁移为按设备档位）：充电档 15s →
    /// max(90, 30)=90s；300s 档 → 600s；默认 1800s 档 → 3600s。
    /// 阈值永不小于两个完整轮询周期。
    #[test]
    fn gray_threshold_anchors_to_device_tier() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config {
            poll_interval_secs: 300,
            experimental_per_device_polling: true,
            ..Config::default()
        })));
        core.upsert_device(dev("a", 1));
        // Normal 档 300s（无读数 / 非充电同档）→ 600s。
        assert_eq!(
            core.gray_threshold_for(&core.entries()[0]),
            Duration::from_secs(600)
        );
        core.record_success("a", reading(30, true), Instant::now()); // 充电档 15s
        assert_eq!(core.gray_threshold_for(&core.entries()[0]), GRAY_AFTER);
        // 1800s 档（充电中仍按充电档）。
        core.swap_config(Config {
            experimental_per_device_polling: true,
            ..Config::default()
        });
        assert_eq!(core.gray_threshold_for(&core.entries()[0]), GRAY_AFTER);
        let mut core2 = AppCore::new(Arc::new(RwLock::new(Config::default())));
        core2.upsert_device(dev("b", 2));
        assert_eq!(
            core2.gray_threshold_for(&core2.entries()[0]),
            Duration::from_secs(3600)
        );
    }

    /// F5 验证断言 A6 之①：poll_interval=300、在线、推进 91s 无轮询 →
    /// 不灰、无 newly_gray（toast 源头）、default_device 仍 Some。
    /// 即 v0.3.15 症状场景在修复后不再误灰误报。
    #[test]
    fn a6_long_poll_91s_without_poll_stays_fresh() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config {
            poll_interval_secs: 300,
            ..Config::default()
        })));
        let t0 = Instant::now();
        core.upsert_device(dev("a", 1));
        core.record_success("a", reading(94, false), t0);
        let later = t0 + Duration::from_secs(91);
        assert!(!core.entries()[0].is_gray(later, core.gray_threshold_for(&core.entries()[0])));
        assert!(core.newly_gray(later).is_empty(), "91s 不得触发灰显通知");
        assert_eq!(core.default_display_index(later), Some(0));
        let snap = core.snapshot(later);
        assert!(!snap.devices[0].gray);
        assert_eq!(snap.default_device, Some(0));
    }

    /// F5 验证断言 A6 之②（WP-C 迁移）：临近到期豁免命中 → 探测轮放行。
    /// 豁免窗口 = 设备自身 next_due 前 30s（默认档 1800s，则 due-30s 起
    /// 豁免生效）；未到窗口、未注册 id、休眠设备（走 D-33 唤醒路径）各
    /// 有边界。豁免与唤醒探测按状态互斥，不会同轮双查。
    #[test]
    fn a6_near_due_exemption_admits_probe_round() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let t0 = Instant::now();
        core.upsert_device(dev("a", 1));
        core.upsert_device(dev("b", 2));
        core.record_success("a", reading(80, false), t0);
        core.record_success("b", reading(60, false), t0);
        // 窗口前（距 due 31s）：两台在线设备均不放行。
        let before = t0 + Duration::from_secs(crate::config::DEFAULT_POLL_SECS - 31);
        assert!(!core.probe_round_allows("a", before));
        assert!(!core.probe_round_allows("b", before));
        assert!(
            !core.probe_round_allows("ghost", before),
            "未注册 id 不放行"
        );
        // 窗口内（距 due 29s）：豁免命中，探测轮放行一次查询。
        let near = t0 + Duration::from_secs(crate::config::DEFAULT_POLL_SECS - 29);
        assert!(core.probe_round_allows("a", near), "临近到期豁免应放行");
        assert!(core.probe_round_allows("b", near));
        // 豁免命中后本轮查到了（豁免的意义）：next_due 按当前档重排，
        // 下一轮窗口重新计时——活跃期读数间隔被收敛到档位上限量级（A3）。
        core.record_success("a", reading(80, false), near);
        assert!(!core.probe_round_allows("a", near + Duration::from_secs(1)));
        // 互斥：b 连续超时进休眠后走 D-33 唤醒路径，豁免对其不生效
        // （准入由 wake_probe_candidate 承担），两路径不会同时放行。
        let _ = core.record_timeout("b", near);
        let _ = core.record_timeout("b", near);
        assert!(core.wake_probe_candidate("b"));
        assert!(!core.near_due_exemption("b", near), "休眠设备不吃在线豁免");
        assert!(
            core.probe_round_allows("b", near),
            "休眠候选仍由唤醒路径放行"
        );
        assert!(!core.wake_probe_candidate("a"), "在线设备不是唤醒候选");
    }

    /// F5 风险回归面②（多设备同接收器）：豁免只放行临近到期的设备，
    /// 不误伤其他设备的节奏；真离线设备按既有路径 超时×2→Sleeping，
    /// 休眠后展示资格保留、灰显最后已知电量——真离线检测链不削弱（A5
    /// 的代码侧可覆盖部分）。
    #[test]
    fn multi_device_exemption_scoped_and_offline_path_intact() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let t0 = Instant::now();
        core.upsert_device(dev("a", 1));
        core.upsert_device(dev("b", 2));
        core.record_success("a", reading(50, false), t0);
        // b 的读数比 a 晚 40s：a 距 due 29s 时进豁免窗口，b 距其 due
        // 还有 69s，尚未进窗口——豁免按设备各自计时，不连坐。
        core.record_success("b", reading(30, false), t0 + Duration::from_secs(40));
        let near = t0 + Duration::from_secs(crate::config::DEFAULT_POLL_SECS - 29);
        assert!(core.probe_round_allows("a", near));
        assert!(
            !core.probe_round_allows("b", near),
            "豁免按设备计时，不连坐"
        );
        // b 真离线：连续两次超时 → Sleeping（既有路径原样）。
        assert_eq!(core.record_timeout("b", near), TimeoutOutcome::Counting(1));
        assert_eq!(core.record_timeout("b", near), TimeoutOutcome::EnteredSleep);
        // 休眠后：展示资格保留（D-33）、灰显最后已知电量；默认展示按
        // 「eligible 中取电量最低」仍选中 b（30% < a 的 50%）——休眠
        // 同权意味着低电休眠设备继续占据默认展示位，不回退「?」。
        let snap = core.snapshot(near);
        assert!(snap.devices[1].gray);
        assert_eq!(snap.devices[1].percent, Some(30));
        assert_eq!(core.default_display_index(near), Some(1));
    }

    /// WP-C：next_due 档位切换——未轮询设备立即到期（自愈）；常规读数
    /// 排到默认档 1800s；充电读数排到充电档 15s。
    #[test]
    fn next_due_switches_when_charging() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config {
            experimental_per_device_polling: true,
            ..Config::default()
        })));
        let now = Instant::now();
        core.upsert_device(dev("a", 1));
        // 未轮询（next_due=None）的非休眠真实设备立即到期：自愈口径，
        // 不因调度字段缺失而永远不查。
        assert!(core.is_due("a", now));
        core.record_success("a", reading(30, false), now);
        assert_eq!(
            core.entries()[0].next_due,
            Some(now + Duration::from_secs(crate::config::DEFAULT_POLL_SECS)),
            "常规读数排到默认档（D-66 DEFAULT_POLL_SECS = 1800s）"
        );
        assert!(!core.is_due("a", now));
        core.record_success("a", reading(30, true), now);
        assert_eq!(
            core.entries()[0].next_due,
            Some(now + POLL_CHARGING),
            "充电读数排到充电档 15s"
        );
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
        assert_eq!(core.record_timeout(id, now), TimeoutOutcome::Counting(1));
        assert_eq!(core.record_timeout(id, now), TimeoutOutcome::EnteredSleep);
        (core, now)
    }

    /// 连续查询超时 → Online → Sleeping；再次超时保持休眠（不 panic）。
    #[test]
    fn consecutive_timeouts_enter_sleep() {
        let (mut core, now) = sleeping_core("a");
        let e = &core.entries()[0];
        assert_eq!(e.device.state, DeviceState::Sleeping);
        assert_eq!(
            core.record_timeout("a", now),
            TimeoutOutcome::AlreadySleeping
        );
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
        core.note_wake_signal("a", now);
        assert_eq!(core.poll_decision("a", now), PollDecision::Probe);
        // 探测开始：资格消耗 + 冷却开始。
        core.begin_wake_probe("a", now);
        assert_eq!(core.poll_decision("a", now), PollDecision::Skip);
        // 冷却窗口内即使再来信号也不立即探测（防信号风暴 = 变相高频查询）。
        core.note_wake_signal("a", now);
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
        core.note_wake_signal("a", now);
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
        core.note_wake_signal("a", now);
        core.begin_wake_probe("a", now);
        assert_eq!(
            core.record_timeout("a", now),
            TimeoutOutcome::AlreadySleeping
        );
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
        assert_eq!(core.record_timeout("a", now), TimeoutOutcome::Counting(1));
        assert_eq!(core.entries()[0].device.state, DeviceState::Online);
        assert_eq!(core.poll_decision("a", now), PollDecision::Probe);
        // 成功后连击清零。
        core.record_success("a", reading(51, false), now);
        assert_eq!(core.record_timeout("a", now), TimeoutOutcome::Counting(1));
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
            core.record_timeout("C52B|usb#1", now),
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
        // （红蓝 P2-2：先经解除沿把 tracker 复位，再调回阈值重造一条
        // 真实在队告警——此前解除沿后队列恒空，all() 对空集恒真、
        // 断言形同虚设。）
        core.swap_config(Config {
            low_threshold: 20,
            ..Config::default()
        });
        assert!(!core.is_low_battery("old"));
        core.swap_config(Config {
            low_threshold: 90,
            ..Config::default()
        });
        assert!(core.is_low_battery("old"), "重估应重新入队一条告警");
        let _ = core.upsert_device(dev_with_unit("new", 0xFF, "AAAA0001"));
        let alerts = core.take_pending_alerts();
        assert_eq!(
            alerts.len(),
            1,
            "rebind 前必须恰有一条在队告警（防空集恒真）"
        );
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

    // ---------- WP-A：虚拟条目（A-1b）/ 键冲突条款（R5-I-01）/ 持久化钩子（A-1） ----------

    fn persisted(key: &str, name: &str, percent: u8) -> PersistedDevice {
        PersistedDevice {
            physical_key: key.into(),
            display_name: name.into(),
            percent,
            charging: false,
            last_seen: 1_700_000_000,
            source: "unified_battery".into(),
        }
    }

    /// 指定显示名构造真实设备（Name 回退键 = 消毒名；键冲突条款测试用）。
    fn dev_named(id: &str, name: &str, index: u8) -> Device {
        let mut d = Device::new(id.into(), name, LinkKind::Bolt, index, false);
        d.set_state(DeviceState::Online);
        d
    }

    /// A-1b 创建段：虚拟条目播种后渲染为「灰色最后已知 + 时间标注」，
    /// 进入菜单与图标候选；零查询守卫全部生效（D-33 红线）。
    #[test]
    fn virtual_seed_renders_gray_last_known_and_never_queried() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let now = Instant::now();
        assert!(core.seed_virtual(&persisted("AAAA0001", "MX Master 3", 55)));

        let snap = core.snapshot(now);
        assert_eq!(snap.devices.len(), 1);
        let row = &snap.devices[0];
        assert_eq!(row.id, "virtual#AAAA0001");
        assert!(row.is_virtual);
        assert_eq!(row.percent, Some(55));
        assert_eq!(row.last_seen, Some(1_700_000_000));
        assert_eq!(row.state, "sleeping");
        assert!(row.gray, "虚拟条目呈灰色最后已知");
        // A-2：自带持久化数据 → 可点（enabled 判定）。
        assert!(row.has_displayable_data());
        // 图标候选：唯一条目即默认展示设备（不是「?」）。
        assert_eq!(snap.default_device, Some(0));

        // 零查询守卫：轮询裁决 Skip、不进休眠装填名单、非唤醒探测候选、
        // 唤醒信号不装填、探测轮不放行。
        let vid = "virtual#AAAA0001";
        assert_eq!(core.poll_decision(vid, now), PollDecision::Skip);
        assert!(core.sleeping_ids().is_empty());
        assert!(!core.wake_probe_candidate(vid));
        assert!(!core.probe_round_allows(vid, now));
        core.note_wake_signal(vid, now);
        assert_eq!(core.poll_decision(vid, now), PollDecision::Skip);
    }

    /// A-1b 播种拒绝：同键已有条目（真实或虚拟）不重复播种。
    #[test]
    fn virtual_seed_skips_existing_entry_same_key() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        // 同键虚拟条目已存在 → 拒绝。
        assert!(core.seed_virtual(&persisted("K1", "Mouse K1", 50)));
        assert!(!core.seed_virtual(&persisted("K1", "Mouse K1", 51)));
        assert_eq!(core.entries().len(), 1);
        // 同键真实条目已存在 → 拒绝（实时链路优先）。
        core.upsert_device(dev_with_unit("ep1", 1, "U1"));
        assert!(!core.seed_virtual(&persisted("U1", "Mouse U1", 60)));
        assert_eq!(core.entries().len(), 2);
    }

    /// A-1b 归并段：真实设备注册按 physical_key 字符串合并虚拟条目
    /// —— proven unitId 与 Name 回退键两种来源都合并；合并后持久化
    /// 最后已知电量保留到首个实时读数接管。
    #[test]
    fn upsert_merges_virtual_entry_by_key_string() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let now = Instant::now();
        // proven unitId 键归并。
        assert!(core.seed_virtual(&persisted("AAAA0001", "MX Master 3", 55)));
        let outcome = core.upsert_device(dev_with_unit("ep9", 3, "AAAA0001"));
        assert_eq!(
            outcome,
            UpsertOutcome::MergedVirtual {
                old_id: "virtual#AAAA0001".into(),
                new_id: "ep9".into()
            }
        );
        assert_eq!(core.entries().len(), 1, "归并不新增条目");
        let e = &core.entries()[0];
        assert_eq!(e.device.id, "ep9");
        assert_eq!(e.device.device_index, 3);
        assert!(!e.is_virtual);
        assert_eq!(e.persisted_last_seen, None);
        assert_eq!(e.device.state, DeviceState::Online);
        assert_eq!(
            e.device.battery.map(|b| b.percent),
            Some(55),
            "实时读数接管前保留持久化最后已知"
        );
        // 首个实时读数接管显示。
        core.record_success("ep9", reading(80, false), now);
        assert_eq!(
            core.entries()[0].device.battery.map(|b| b.percent),
            Some(80)
        );

        // Name 回退键归并（同名字符串、无 provenance）。
        let mut core2 = AppCore::new(Arc::new(RwLock::new(Config::default())));
        assert!(core2.seed_virtual(&persisted("Mouse X", "Mouse X", 40)));
        let outcome2 = core2.upsert_device(dev_named("epX", "Mouse X", 2));
        assert!(matches!(outcome2, UpsertOutcome::MergedVirtual { .. }));
        assert_eq!(core2.entries().len(), 1);
        assert!(!core2.entries()[0].is_virtual);
    }

    /// 小修批第 2 项（P2-1 修复）：注册前离线判定必须捕获 upsert 前
    /// 状态——虚拟条目（Sleeping）在 upsert 前判 true；upsert 归并后
    /// 条目状态已迁 Online，同键再判为 false。这正是 v0.3.23 真机
    /// wake_recovery_via_reenum 恒 0 的根因场景（原实现在 upsert 后
    /// 读状态，MergedVirtual 路径恒判在线）。
    #[test]
    fn pre_registration_offline_captures_state_before_upsert() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        assert!(core.seed_virtual(&persisted("AAAA0001", "MX Master 3", 55)));
        let d = dev_with_unit("ep9", 3, "AAAA0001");
        assert!(
            core.pre_registration_offline(&d),
            "虚拟条目 Sleeping → upsert 前判离线"
        );
        let outcome = core.upsert_device(d.clone());
        assert!(matches!(outcome, UpsertOutcome::MergedVirtual { .. }));
        assert!(
            !core.pre_registration_offline(&d),
            "归并后状态已迁 Online → 不再判离线（旧实现在此恒 false）"
        );
    }

    /// 小修批第 2 项：同端点 id（Updated 路径，真实条目 Sleeping）与
    /// 全新插入（Inserted，无在册条目）的判定边界。
    #[test]
    fn pre_registration_offline_updated_and_inserted_paths() {
        let (mut core, _now) = sleeping_core("a");
        // 同 id 在册且 Sleeping → true。
        assert!(core.pre_registration_offline(&dev("a", 1)));
        // 全新插入（无在册条目）→ false。
        assert!(!core.pre_registration_offline(&dev("zzz", 9)));
        // Online 在册 → false。
        core.upsert_device(dev("b", 2));
        assert!(!core.pre_registration_offline(&dev("b", 2)));
    }

    /// R5-I-01 键冲突条款：≥2 个真实条目共享同一 Name 回退键 → 冲突；
    /// 冲突设备不入持久化、不生成虚拟条目；虚拟条目不计入冲突数
    /// （虚拟 + 真实同键 = 待归并）；设备拔出后冲突解除。
    #[test]
    fn key_conflict_detection_and_three_package_behavior() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let now = Instant::now();
        let mut store =
            LastKnownStore::new(std::env::temp_dir().join("vt-core-conflict-test.json"));

        // 虚拟 + 真实同键：待归并，不是冲突。
        assert!(core.seed_virtual(&persisted("Mouse Duo", "Mouse Duo", 70)));
        core.upsert_device(dev_named("epA", "Mouse Duo", 1)); // 归并虚拟条目
        assert!(core.refresh_key_conflicts().is_empty());

        // 第二台同名真实设备注册（unitId 均取不到 → 同 Name 回退键）。
        core.upsert_device(dev_named("epB", "Mouse Duo", 2));
        assert_eq!(core.refresh_key_conflicts(), vec!["Mouse Duo".to_string()]);
        // 幂等：未变化时不重复报告（日志只记一次的语义基础）。
        assert!(core.refresh_key_conflicts().is_empty());

        // ① 不入持久化：store 同步冲突集合后，成功读数不落记录。
        store.set_conflicted(core.conflicted_keys().clone());
        core.record_success("epA", reading(30, false), now);
        core.record_success("epB", reading(31, false), now);
        assert!(!core.persist_note_success(&mut store, "epA", 1_700_000_100, now));
        assert!(!core.persist_note_success(&mut store, "epB", 1_700_000_100, now));
        assert!(store.records().is_empty());

        // ② 不生成虚拟条目。
        assert!(!core.seed_virtual(&persisted("Mouse Duo", "Mouse Duo", 65)));

        // ③ 实时读数正常显示（持久化弃用不影响实时链路）。
        let snap = core.snapshot(now);
        assert_eq!(snap.devices.len(), 2);
        assert!(snap.devices.iter().all(|d| !d.is_virtual));
        assert!(snap.devices.iter().all(|d| d.percent.is_some()));

        // 设备拔出 → 冲突解除 → 恢复可写。
        core.remove_device("epB");
        assert!(core.refresh_key_conflicts().is_empty());
        store.set_conflicted(core.conflicted_keys().clone());
        assert!(core.persist_note_success(&mut store, "epA", 1_700_000_200, now));
    }

    /// R5-I-01 红队复核建议：检测到冲突时同键存量虚拟条目被清除
    /// （不展示持久化最后已知）；真实条目不被动。
    #[test]
    fn conflict_detection_removes_existing_virtual_entry() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        // 一台设备时期正常持久化 → 重启后播种虚拟条目。
        assert!(core.seed_virtual(&persisted("Mouse Duo", "Mouse Duo", 70)));
        // 两台同名真实设备同时出现（归并一台 + 新增一台）→ 冲突。
        core.upsert_device(dev_named("epA", "Mouse Duo", 1));
        core.upsert_device(dev_named("epB", "Mouse Duo", 2));
        assert_eq!(core.entries().len(), 2, "虚拟已归并一台");
        // 另一种时序：虚拟尚未归并即检出冲突（两台先注册、播种在后
        // 被冲突拦截已覆盖；此处验「虚拟在册 + 冲突形成」的清除路径）。
        let mut core2 = AppCore::new(Arc::new(RwLock::new(Config::default())));
        assert!(core2.seed_virtual(&persisted("Mouse Duo", "Mouse Duo", 70)));
        // 手工构造「虚拟在册 + 两真实同键」：先注册两台同名真实设备
        // （播种在前、冲突在后——真实设备归并走键相等，先归并一台，
        // 第二台新增后冲突形成；虚拟已不在册）。为覆盖「虚拟仍在册时
        // 冲突形成」路径，直接用 remove_virtual_by_keys 验证清除语义：
        let removed = core2.remove_virtual_by_keys(&["Mouse Duo".to_string()]);
        assert_eq!(removed, vec!["virtual#Mouse Duo".to_string()]);
        assert!(core2.entries().is_empty());
    }

    /// A-1 持久化钩子：真实设备成功读数收敛入库；虚拟条目永不产生
    /// 持久化写入。
    #[test]
    fn persist_hook_writes_real_readings_only() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let now = Instant::now();
        let mut store = LastKnownStore::new(std::env::temp_dir().join("vt-core-hook-test.json"));

        core.upsert_device(dev_with_unit("ep1", 1, "AAAA0001"));
        core.record_success("ep1", reading(55, false), now);
        assert!(core.persist_note_success(&mut store, "ep1", 1_700_000_000, now));
        let recs = store.records();
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].physical_key, "AAAA0001");
        assert_eq!(recs[0].percent, 55);

        // 同值再读：last_seen 刷新但不重复标 dirty（变化驱动）。
        core.record_success("ep1", reading(55, false), now);
        assert!(!core.persist_note_success(&mut store, "ep1", 1_700_000_030, now));

        // 虚拟条目：钩子直接拒绝。
        assert!(core.seed_virtual(&persisted("K9", "Mouse K9", 66)));
        assert!(!core.persist_note_success(&mut store, "virtual#K9", 1_700_000_040, now));
        assert_eq!(store.records().len(), 1);
    }

    /// A-1b 清除段：LRU/字节上限淘汰或用户移除时，虚拟条目随持久化
    /// 记录一并清除；真实条目不受影响。
    #[test]
    fn remove_virtual_by_keys_clears_only_virtual() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let now = Instant::now();
        assert!(core.seed_virtual(&persisted("Mouse K1", "Mouse K1", 50)));
        assert!(core.seed_virtual(&persisted("K2", "Mouse K2", 60)));
        core.upsert_device(dev_named("epR", "Mouse K1", 1)); // 归并 Mouse K1
        core.record_success("epR", reading(77, false), now);

        // K2 被淘汰：清虚拟条目；Mouse K1 已归并为真实条目——即便其键
        // 列入清除名单也不被动（is_virtual 守卫）。
        let removed = core.remove_virtual_by_keys(&["K2".to_string(), "Mouse K1".to_string()]);
        assert_eq!(removed, vec!["virtual#K2".to_string()]);
        assert_eq!(core.entries().len(), 1);
        assert_eq!(core.entries()[0].device.id, "epR");
        assert!(!core.entries()[0].is_virtual);
    }

    // —— WP-C：按设备计时轮询（v3 C-1 / AC-C1 / AC-C2 / I-06）——

    /// AC-C1：充电档 15s 节拍——充电读数后 next_due = now+15s；+13s 未
    /// 到期、+15s 到期（±2s 容差由调度侧节拍保证，核心侧断言到期点本身
    /// 精确落在 15s）。充电设备被排除在临近到期豁免之外：due 前 10s
    /// 已在 30s 豁免窗内也不放行——30s 提前量会破坏 15s±2s 节拍。
    #[test]
    fn wp_c_charging_tier_15s_cadence() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config {
            experimental_per_device_polling: true,
            ..Config::default()
        })));
        let t0 = Instant::now();
        core.upsert_device(dev("a", 1));
        core.record_success("a", reading(60, true), t0);
        assert_eq!(core.entries()[0].next_due, Some(t0 + POLL_CHARGING));
        assert!(
            !core.is_due("a", t0 + Duration::from_secs(13)),
            "+13s 未到期"
        );
        assert!(core.is_due("a", t0 + POLL_CHARGING), "+15s 到期");
        // 节拍保持：每到都读到充电 → 下一个到期点恒为「本次读数 +15s」。
        let t1 = t0 + POLL_CHARGING;
        core.record_success("a", reading(61, true), t1);
        assert_eq!(core.entries()[0].next_due, Some(t1 + POLL_CHARGING));
        // 充电设备不吃临近到期豁免（due 前 10s 在 30s 窗内也不放行）。
        let near = t1 + Duration::from_secs(5);
        assert!(!core.near_due_exemption("a", near), "充电档排除在豁免外");
        assert!(!core.probe_round_allows("a", near));
    }

    /// AC-C2：拔线 ≤15s 感知——无事件监听，靠充电档 15s 节拍：拔线后
    /// 第一次充电档读数（距上次充电读数 ≤15s）读到 not-charging，
    /// next_due 立即跳到常规档 1800s。
    #[test]
    fn wp_c_unplug_perceived_within_15s() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config {
            experimental_per_device_polling: true,
            ..Config::default()
        })));
        let t0 = Instant::now();
        core.upsert_device(dev("a", 1));
        core.record_success("a", reading(70, true), t0);
        // 拔线发生在 t0 之后任意时刻；下一次充电档读数最迟 t0+15s 发出。
        let read_at = t0 + POLL_CHARGING;
        assert!(core.is_due("a", read_at), "充电档保证 ≤15s 内有读数");
        core.record_success("a", reading(70, false), read_at);
        assert_eq!(
            core.entries()[0].next_due,
            Some(read_at + Duration::from_secs(crate::config::DEFAULT_POLL_SECS)),
            "读到拔线即切常规档"
        );
    }

    /// I-06：per-device 互不影响——a 充电 15s 节拍不抬高 b 的常规档
    /// 节奏；b 的到期点不随 a 的读数移动。睡眠预算的设备侧输入
    /// next_poll_due = 全设备最近到期点（此处即 a 的 15s 到期点）。
    #[test]
    fn wp_c_per_device_independence() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config {
            experimental_per_device_polling: true,
            ..Config::default()
        })));
        let t0 = Instant::now();
        core.upsert_device(dev("a", 1));
        core.upsert_device(dev("b", 2));
        core.record_success("a", reading(50, true), t0); // 充电档
        core.record_success("b", reading(40, false), t0); // 常规档
        assert_eq!(core.next_poll_due(), Some(t0 + POLL_CHARGING));
        let t1 = t0 + POLL_CHARGING;
        assert!(core.is_due("a", t1));
        assert!(!core.is_due("b", t1), "a 的充电节拍不抬高 b");
        // any_due：任一在线设备到期即 true（整轮 vs 仅探测轮裁决输入）；
        // a 未到期、b 未到期时两台都不应触发整轮。
        assert!(core.any_due(t1), "a 到期 → 有设备应查");
        let t_half = t0 + Duration::from_secs(7);
        assert!(!core.any_due(t_half), "两台均未到期 → 不强制整轮");
        core.record_success("a", reading(51, true), t1);
        assert_eq!(
            core.entries()[1].next_due,
            Some(t0 + Duration::from_secs(crate::config::DEFAULT_POLL_SECS)),
            "b 的到期点不受 a 读数影响"
        );
    }

    /// v3 C-1 睡眠预算输入：休眠设备 next_due=None（不占预算）；
    /// 唤醒信号把本设备到期点提到「尽快」（冷却窗口内排在冷却结束点，
    /// per-entry 冷却语义 I-12 不变）；探测在途摘除排程。
    #[test]
    fn wp_c_sleep_budget_inputs() {
        let (mut core, now) = sleeping_core("a");
        assert_eq!(core.entries()[0].next_due, None, "休眠不占睡眠预算");
        assert_eq!(core.next_poll_due(), None, "无排程设备 → None");
        // 唤醒信号：无冷却负担 → 立即到期。
        core.note_wake_signal("a", now);
        assert_eq!(core.entries()[0].next_due, Some(now));
        // 探测在途：摘除排程（结果会重新排程）。
        core.begin_wake_probe("a", now);
        assert_eq!(core.entries()[0].next_due, None);
        // 探测超时保持休眠；冷却窗口内再来信号 → 排在冷却结束点。
        let _ = core.record_timeout("a", now);
        let within = now + Duration::from_secs(1);
        core.note_wake_signal("a", within);
        assert_eq!(
            core.entries()[0].next_due,
            Some(now + WAKE_PROBE_COOLDOWN),
            "冷却 per-entry：冷却内的信号排到冷却结束点"
        );
    }

    /// WP-C 重排钩子：非超时非 gone 的硬失败按本设备当前档位推后
    /// next_due，防止到期点滞留形成紧循环；休眠设备不动（D-33）。
    #[test]
    fn wp_c_hard_failure_reschedules() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let t0 = Instant::now();
        core.upsert_device(dev("a", 1));
        core.record_success("a", reading(50, false), t0);
        let t1 = t0 + Duration::from_secs(5);
        core.reschedule("a", t1);
        assert_eq!(
            core.entries()[0].next_due,
            Some(t1 + Duration::from_secs(crate::config::DEFAULT_POLL_SECS)),
            "硬失败按当前档位重排"
        );
        // 休眠设备：重排钩子跳过（排程归 D-33 唤醒路径管理）。
        let (mut core2, now2) = sleeping_core("b");
        core2.reschedule("b", now2);
        assert_eq!(core2.entries()[0].next_due, None);
    }

    /// 多设备同接收器回归（WP-C 后）：两台同接收器设备各自排程、
    /// 互不占预算；一台休眠后 next_poll_due 由另一台接管；快照与
    /// 默认展示不受影响。
    #[test]
    fn wp_c_multi_device_same_receiver_regression() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let t0 = Instant::now();
        core.upsert_device(dev("a", 1));
        core.upsert_device(dev("b", 2));
        core.record_success("a", reading(50, false), t0);
        core.record_success("b", reading(30, false), t0 + Duration::from_secs(60));
        assert_eq!(
            core.next_poll_due(),
            Some(t0 + Duration::from_secs(crate::config::DEFAULT_POLL_SECS)),
            "最近到期点 = a 的常规档到期点"
        );
        // a 超时×2 进休眠 → 不占预算，next_poll_due 由 b 接管。
        let t1 = t0 + Duration::from_secs(120);
        let _ = core.record_timeout("a", t1);
        let _ = core.record_timeout("a", t1);
        assert_eq!(
            core.next_poll_due(),
            Some(
                t0 + Duration::from_secs(60)
                    + Duration::from_secs(crate::config::DEFAULT_POLL_SECS)
            ),
            "a 休眠后预算由 b 的到期点接管"
        );
        // 快照与默认展示回归：两台都在册，b 低电占默认展示位。
        let snap = core.snapshot(t1);
        assert_eq!(snap.devices.len(), 2);
        assert_eq!(snap.default_device, Some(1));
        assert!(snap.devices[1].percent.is_some());
    }

    /// D-74（相位二第 5 项）：retier_all 换入新配置后全体在线设备按新档
    /// 即时重排；休眠/虚拟设备恒 None（D-33 零查询红线不受重排影响）。
    #[test]
    fn retier_all_reschedules_online_devices_immediately() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let t0 = Instant::now();
        core.upsert_device(dev("a", 1));
        core.upsert_device(dev("b", 2));
        core.record_success("a", reading(50, false), t0);
        core.record_success("b", reading(60, false), t0);
        // 换入 600s 档并重排 → 两台在线设备到期点 = t1 + 600s。
        let t1 = t0 + Duration::from_secs(10);
        core.swap_config(Config {
            poll_interval_secs: 600,
            ..Config::default()
        });
        core.retier_all(t1);
        assert_eq!(
            core.entries[0].next_due,
            Some(t1 + Duration::from_secs(600))
        );
        assert_eq!(
            core.entries[1].next_due,
            Some(t1 + Duration::from_secs(600))
        );
        // 休眠设备：重排后恒 None（不排程、零查询）。
        core.entries[0].device.set_state(DeviceState::Sleeping);
        core.retier_all(t1);
        assert_eq!(core.entries[0].next_due, None);
        assert_eq!(
            core.entries[1].next_due,
            Some(t1 + Duration::from_secs(600))
        );
    }

    /// 相位二（第 4 项）：experimental_per_device_polling 门控——开时充电
    /// 设备 15s 快档，关时退回配置档；休眠恒 None 与开关无关（D-33）。
    #[test]
    fn tier_interval_respects_experimental_per_device_polling() {
        let mut core = AppCore::new(Arc::new(RwLock::new(Config::default())));
        let now = Instant::now();
        core.upsert_device(dev("a", 1));
        core.record_success("a", reading(50, true), now); // 充电中
        let cfg_on = Config {
            experimental_per_device_polling: true,
            ..Config::default()
        };
        assert_eq!(core.entries[0].tier_interval(&cfg_on), Some(POLL_CHARGING));
        // 显式置关（v0.3.22 起新装默认为开，D-85）：充电设备退回配置档。
        let cfg_off = Config {
            experimental_per_device_polling: false,
            ..Config::default()
        };
        assert_eq!(
            core.entries[0].tier_interval(&cfg_off),
            Some(Duration::from_secs(cfg_off.poll_interval_secs))
        );
        // 休眠恒 None，与开关无关。
        core.entries[0].device.set_state(DeviceState::Sleeping);
        assert_eq!(core.entries[0].tier_interval(&cfg_on), None);
        assert_eq!(core.entries[0].tier_interval(&cfg_off), None);
    }
}
