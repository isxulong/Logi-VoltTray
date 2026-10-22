//! 持久化最后已知电量（WP-A A-1）：`%LOCALAPPDATA%\LogiVoltTray\last_known.json`。
//!
//! 设计基线 = v3 文档 A-1 节，要点：
//! - 单文件 JSON `{version, devices:[{physical_key, display_name, percent,
//!   charging, last_seen, source}]}`，**不与 config.toml 同目录**（roaming
//!   profile 风险，v3 修订自 %APPDATA% 迁出）；
//! - 有界：条目 ≤ [`MAX_DEVICES`]（last_seen LRU 淘汰），文件 ≤ [`MAX_BYTES`]
//!   （防失控保险，触顶按同一 LRU 淘汰至回落），display_name ≤ [`MAX_NAME_CHARS`]
//!   字符截断；淘汰检查双挂——启动加载时 + 每次落盘前；不做时间过期淘汰；
//! - 变化驱动：成功读数且值（percent/charging）有变化才标 dirty；防抖
//!   同一设备 ≤1/30s、全局 ≤1/10s；dirty 后 60s 内 OS flush 兜底；fsync
//!   退出时 + 每 5 分钟。崩溃最坏丢 60s、掉电最坏丢 5 分钟（统一口径）；
//! - 原子写：同目录 `.tmp` → flush → rename；启动清理孤儿 `*.tmp`；
//! - 损坏容错：解析失败 / version 不支持 → 改名 `.bak`（占第二文件位，
//!   文件数恒 ≤2）→ 回退「?」不阻塞启动 → 下次成功读数后重建（AC-A7）；
//! - 回退键冲突（R5-I-01）：冲突键不入库、存量记录即时清除、不使用
//!   （`set_conflicted`；显示层不生成虚拟条目由 core 配合）。
//!
//! 本模块只做主机侧文件 I/O 与纯逻辑，零设备查询（电池红线）。
//! WP-B 预留：[`FlushTuning`] 即 v3 所称「落盘协调器」的节拍参数集，
//! stats.jsonl 的防抖/兜底/fsync 口径整体继承本文件（v3 WP-B 节
//! 「落盘频率与丢失口径整体继承 A-1」）。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// 持久化 schema 版本（version 不匹配按损坏处理——结构变更即换格式）。
pub const SCHEMA_VERSION: u32 = 1;
/// 持久化条目上限（文件内 LRU 主淘汰机制，v3 A-1）。
pub const MAX_DEVICES: usize = 32;
/// 文件大小保险上限（触顶后按同一 LRU 规则淘汰至回落，v3 A-1）。
pub const MAX_BYTES: usize = 256 * 1024;
/// display_name 长度上限（字符数，超长截断，v3 A-1 字段级约束）。
pub const MAX_NAME_CHARS: usize = 64;
/// 防抖：同一设备两次持久化写入的最小间隔（v3 A-1）。
pub const PER_DEVICE_DEBOUNCE: Duration = Duration::from_secs(30);
/// 防抖：全局两次落盘的最小间隔（v3 A-1）。
pub const GLOBAL_DEBOUNCE: Duration = Duration::from_secs(10);
/// dirty 后 OS flush 兜底时限（崩溃最坏丢 60s 的统一口径来源）。
pub const FLUSH_FALLBACK: Duration = Duration::from_secs(60);
/// fsync 定时间隔（退出时无条件 fsync 一次，掉电最坏丢 5 分钟）。
pub const FSYNC_INTERVAL: Duration = Duration::from_secs(300);

/// 持久化文件名 / 备份名 / 目录名（v3 A-1）。
pub const STORE_FILE_NAME: &str = "last_known.json";
pub const STORE_BACKUP_NAME: &str = "last_known.json.bak";
pub const STORE_DIR_NAME: &str = "LogiVoltTray";

/// 落盘节拍参数集（WP-B 落盘协调器预留：同一套防抖/兜底/fsync 口径）。
/// 生产用 [`FlushTuning::default`]（即 v3 声明上限）；测试用小参数注入。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlushTuning {
    /// 同一设备两次持久化写入的最小间隔。
    pub per_device_debounce: Duration,
    /// 全局两次落盘的最小间隔。
    pub global_debounce: Duration,
    /// dirty 后 OS flush 兜底时限。
    pub flush_fallback: Duration,
    /// fsync 定时间隔。
    pub fsync_interval: Duration,
}

impl Default for FlushTuning {
    fn default() -> Self {
        FlushTuning {
            per_device_debounce: PER_DEVICE_DEBOUNCE,
            global_debounce: GLOBAL_DEBOUNCE,
            flush_fallback: FLUSH_FALLBACK,
            fsync_interval: FSYNC_INTERVAL,
        }
    }
}

/// 默认存储路径：`%LOCALAPPDATA%\LogiVoltTray\last_known.json`
///（无 LOCALAPPDATA 时退回当前目录，与 config.default_path 同款兜底）。
pub fn default_path() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(STORE_DIR_NAME).join(STORE_FILE_NAME)
}

/// 单台设备的持久化记录（schema 与 v3 A-1 一致；last_seen 为 epoch 秒）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PersistedDevice {
    /// 物理身份键字符串（unitId 优先、回退消毒设备名；不记 provenance）。
    pub physical_key: String,
    /// 显示名（写入时按 [`MAX_NAME_CHARS`] 截断）。
    pub display_name: String,
    pub percent: u8,
    pub charging: bool,
    /// 最后一次成功读数的墙钟时刻（epoch 秒；LRU 与时间标注用）。
    pub last_seen: u64,
    /// 电量来源（`BatterySource::name()` 字符串）。
    pub source: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct LastKnownFile {
    version: u32,
    devices: Vec<PersistedDevice>,
}

/// 加载结果（调用方据此记日志 / 决定是否重建）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadOutcome {
    /// 文件不存在（首次运行）——空库起步，不算异常。
    Empty,
    /// 成功读回 N 条记录（启动淘汰检查后）。
    Loaded(usize),
    /// 文件损坏 / version 不支持 / 不可读——已尽力改名 `.bak` 保留证据，
    /// 回退空库（「?」），不阻塞启动；下次成功读数后重建（AC-A7）。
    Recovered,
}

/// 一次落盘尝试的结果。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FlushOutcome {
    /// 本次是否实际写盘。
    pub wrote: bool,
    /// 本次是否执行了 fsync。
    pub fsynced: bool,
    /// 本次被 LRU / 256KB 保险淘汰的记录键（调用方同步清虚拟条目）。
    pub evicted: Vec<String>,
}

/// 持久化存储（单文件、有界、原子写、防抖落盘）。
///
/// 时间注入：`wall_secs`（epoch 秒，写入 last_seen）与 `now`（Instant，
/// 防抖/fsync 节拍）全部由调用方给出，跨平台可测。
#[derive(Debug)]
pub struct LastKnownStore {
    path: PathBuf,
    tuning: FlushTuning,
    /// 当前内存记录（键 = physical_key 字符串）。
    records: HashMap<String, PersistedDevice>,
    /// 最近一次成功落盘的内容镜像（dirty 判定与淘汰的基准）。
    persisted: HashMap<String, PersistedDevice>,
    /// 值有变化、等待落盘的键 → 首次标 dirty 的时刻（60s 兜底判定）。
    dirty_since: HashMap<String, Instant>,
    /// 每设备最近一次被持久化（落盘写入）的时刻（30s 防抖）。
    last_persisted_at: HashMap<String, Instant>,
    /// 最近一次落盘时刻（10s 全局防抖）。
    last_flush: Option<Instant>,
    /// 距上次 fsync 以来的落盘次数。
    writes_since_fsync: u32,
    /// 最近一次 fsync 时刻（5min 定时）。
    last_fsync: Option<Instant>,
    /// 回退键冲突集合（R5-I-01）：永不入库、不使用存量记录。
    conflicted: HashSet<String>,
    /// 实验开关（experimental_persist_last_known）关闭态：false 时全部
    /// 读写在入口即 no-op——不加载存量、不播种虚拟条目、不落盘（相位二
    /// 第 4 项：实验功能新装默认关，关闭即彻底停用而非静默继续）。
    enabled: bool,
}

impl LastKnownStore {
    /// 生产构造（默认节拍）。
    pub fn new(path: PathBuf) -> Self {
        Self::with_tuning(path, FlushTuning::default())
    }

    /// 测试 / WP-B 复用构造（注入节拍）。
    pub fn with_tuning(path: PathBuf, tuning: FlushTuning) -> Self {
        LastKnownStore {
            path,
            tuning,
            records: HashMap::new(),
            persisted: HashMap::new(),
            dirty_since: HashMap::new(),
            last_persisted_at: HashMap::new(),
            last_flush: None,
            writes_since_fsync: 0,
            last_fsync: None,
            conflicted: HashSet::new(),
            enabled: true,
        }
    }

    /// 实验开关关闭构造（相位二第 4 项）：空库 + enabled=false，
    /// records()/note_reading()/set_conflicted()/remove()/flush 全部
    /// no-op；调用方跳过 load（不读存量、不触发 .bak 改名副作用）。
    pub fn disabled(path: PathBuf) -> Self {
        let mut s = Self::with_tuning(path, FlushTuning::default());
        s.enabled = false;
        s
    }

    /// 实验开关状态（诊断导出 / 测试断言用）。
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 当前内存记录（虚拟条目播种 / 调试导出用）。
    pub fn records(&self) -> Vec<PersistedDevice> {
        if !self.enabled {
            return Vec::new();
        }
        let mut v: Vec<_> = self.records.values().cloned().collect();
        v.sort_by(|a, b| a.physical_key.cmp(&b.physical_key));
        v
    }

    /// 冲突键集合（core 显示层判定用）。
    pub fn conflicted(&self) -> &HashSet<String> {
        &self.conflicted
    }

    /// 加载（启动路径）：
    /// 1. 清理同目录孤儿 `*.tmp`（原子写中途崩溃的残留）；
    /// 2. 文件不存在 → Empty；
    /// 3. 解析失败 / version 不支持 → 改名 `.bak`（覆盖旧 bak，文件数 ≤2）
    ///    → Recovered；其他 I/O 错误（权限等）→ Recovered（不改名，保留原文件）；
    /// 4. 成功：逐条消毒（空键丢弃、display_name 截断、同键去重保留
    ///    last_seen 最新），再做启动淘汰检查（32 条 + 256KB 双挂之一），
    ///    内存库与落盘镜像同时就位（启动淘汰的结果视为当前已持久化状态，
    ///    下一次落盘自然写回淘汰后内容）。
    pub fn load(path: PathBuf, tuning: FlushTuning) -> (Self, LoadOutcome) {
        let mut store = Self::with_tuning(path, tuning);
        store.cleanup_orphan_tmps();
        let raw = match fs::read_to_string(&store.path) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return (store, LoadOutcome::Empty);
            }
            Err(_) => {
                // 权限 / 不可读：回退空库但不改名原文件（改名多半也会失败，
                // 且文件本身未必损坏，留给下次启动重试）。
                return (store, LoadOutcome::Recovered);
            }
        };
        let parsed: Result<LastKnownFile, _> = serde_json::from_str(&raw);
        let file = match parsed {
            Ok(f) if f.version == SCHEMA_VERSION => f,
            _ => {
                // 损坏 / version 不支持：改名 .bak 保留证据（AC-A7），
                // 覆盖旧 bak（rename 在 Windows 为 MOVEFILE_REPLACE_EXISTING，
                // 文件数恒 ≤2）。
                let bak = store.path.with_file_name(STORE_BACKUP_NAME);
                let _ = fs::rename(&store.path, &bak);
                return (store, LoadOutcome::Recovered);
            }
        };
        let mut map: HashMap<String, PersistedDevice> = HashMap::new();
        for mut rec in file.devices {
            rec.display_name = truncate_name(&rec.display_name);
            if rec.physical_key.is_empty() {
                continue;
            }
            // 同键去重：保留 last_seen 最新一条（防御手改文件）。
            match map.get(&rec.physical_key) {
                Some(old) if old.last_seen >= rec.last_seen => {}
                _ => {
                    map.insert(rec.physical_key.clone(), rec);
                }
            }
        }
        let mut records: Vec<PersistedDevice> = map.into_values().collect();
        enforce_caps(&mut records);
        store.records = records
            .into_iter()
            .map(|r| (r.physical_key.clone(), r))
            .collect();
        store.persisted = store.records.clone();
        let n = store.records.len();
        (store, LoadOutcome::Loaded(n))
    }

    /// 变化驱动采集（A-1「成功读到电量且值有变化 → 标 dirty」）。
    ///
    /// 每次成功读数都会刷新内存记录的 last_seen（LRU 保鲜，借下一次
    /// 落盘自然写回），但只有 percent / charging / source 变化（或新键）
    /// 才标 dirty。冲突键永不入库（R5-I-01）。返回是否新标 dirty。
    #[allow(clippy::too_many_arguments)]
    pub fn note_reading(
        &mut self,
        physical_key: &str,
        display_name: &str,
        percent: u8,
        charging: bool,
        source: &str,
        wall_secs: u64,
        now: Instant,
    ) -> bool {
        if !self.enabled || physical_key.is_empty() || self.conflicted.contains(physical_key) {
            return false;
        }
        let name = truncate_name(display_name);
        let changed = match self.records.get_mut(physical_key) {
            Some(rec) => {
                let changed =
                    rec.percent != percent || rec.charging != charging || rec.source != source;
                rec.percent = percent;
                rec.charging = charging;
                rec.source = source.to_string();
                rec.display_name = name;
                rec.last_seen = wall_secs;
                changed
            }
            None => {
                self.records.insert(
                    physical_key.to_string(),
                    PersistedDevice {
                        physical_key: physical_key.to_string(),
                        display_name: name,
                        percent,
                        charging,
                        last_seen: wall_secs,
                        source: source.to_string(),
                    },
                );
                true
            }
        };
        if changed {
            self.dirty_since
                .entry(physical_key.to_string())
                .or_insert(now);
        }
        changed
    }

    /// 同步回退键冲突集合（R5-I-01 + 红队复核建议：检测到冲突即停写
    /// 该键并**不再使用其存量记录**）。返回被即时清除的存量记录键
    /// （调用方据此清虚拟条目）。冲突解除（集合收缩）时恢复可写。
    pub fn set_conflicted(&mut self, keys: HashSet<String>) -> Vec<String> {
        if !self.enabled {
            return Vec::new();
        }
        let mut removed = Vec::new();
        for k in keys.difference(&self.conflicted) {
            if self.records.remove(k).is_some() | self.persisted.remove(k).is_some() {
                removed.push(k.clone());
            }
            self.dirty_since.remove(k);
            self.last_persisted_at.remove(k);
        }
        self.conflicted = keys;
        removed
    }

    /// 移除记录（LRU 淘汰同步 / 用户移除路径；返回实际移除的键）。
    pub fn remove(&mut self, keys: &[String]) -> Vec<String> {
        if !self.enabled {
            return Vec::new();
        }
        let mut removed = Vec::new();
        for k in keys {
            if self.records.remove(k).is_some() | self.persisted.remove(k).is_some() {
                removed.push(k.clone());
            }
            self.dirty_since.remove(k);
            self.last_persisted_at.remove(k);
        }
        removed
    }

    /// 防抖落盘（IO 循环每 tick 调用一次）：
    /// - 无 dirty → 仅检查 fsync 定时点；
    /// - 全局 ≤1/10s 且（就绪设备 = 距上次持久化 ≥30s；兜底 = 最老 dirty
    ///   已等 ≥60s 时全部 dirty 就绪）→ 落盘；
    /// - 落盘后 writes_since_fsync>0 且距上次 fsync ≥5min → fsync。
    pub fn flush_if_due(&mut self, now: Instant) -> io::Result<FlushOutcome> {
        let mut outcome = FlushOutcome::default();
        if !self.enabled {
            return Ok(outcome);
        }
        if !self.dirty_since.is_empty() {
            let global_ok = self
                .last_flush
                .is_none_or(|t| now.duration_since(t) >= self.tuning.global_debounce);
            if global_ok {
                let oldest_dirty = self.dirty_since.values().min().copied();
                let fallback = oldest_dirty
                    .is_some_and(|t| now.duration_since(t) >= self.tuning.flush_fallback);
                let ready: Vec<String> = self
                    .dirty_since
                    .keys()
                    .filter(|k| {
                        fallback
                            || self.last_persisted_at.get(*k).is_none_or(|t| {
                                now.duration_since(*t) >= self.tuning.per_device_debounce
                            })
                    })
                    .cloned()
                    .collect();
                if !ready.is_empty() {
                    outcome = self.flush_keys(&ready, now)?;
                }
            }
        }
        // fsync 定时点（有未 fsync 的落盘才需要做）。
        if self.writes_since_fsync > 0
            && self
                .last_fsync
                .is_none_or(|t| now.duration_since(t) >= self.tuning.fsync_interval)
        {
            self.fsync_file()?;
            self.last_fsync = Some(now);
            self.writes_since_fsync = 0;
            outcome.fsynced = true;
        }
        Ok(outcome)
    }

    /// 退出路径：强制落盘全部 dirty + fsync（v3「fsync 退出时执行」）。
    pub fn shutdown_flush(&mut self, now: Instant) -> io::Result<FlushOutcome> {
        if !self.enabled {
            return Ok(FlushOutcome::default());
        }
        let keys: Vec<String> = self.dirty_since.keys().cloned().collect();
        let mut outcome = if keys.is_empty() {
            FlushOutcome::default()
        } else {
            self.flush_keys(&keys, now)?
        };
        if self.writes_since_fsync > 0 {
            self.fsync_file()?;
            self.last_fsync = Some(now);
            self.writes_since_fsync = 0;
        }
        outcome.fsynced = true;
        Ok(outcome)
    }

    /// 实际落盘：合并就绪键 → 淘汰检查（双挂之一）→ 原子写
    ///（`.tmp` → flush → rename）。返回淘汰键供调用方同步虚拟条目。
    fn flush_keys(&mut self, keys: &[String], now: Instant) -> io::Result<FlushOutcome> {
        for k in keys {
            if let Some(rec) = self.records.get(k) {
                self.persisted.insert(k.clone(), rec.clone());
            }
            self.dirty_since.remove(k);
            self.last_persisted_at.insert(k.clone(), now);
        }
        let mut all: Vec<PersistedDevice> = self.persisted.values().cloned().collect();
        let evicted = enforce_caps(&mut all);
        for k in &evicted {
            self.persisted.remove(k);
            self.records.remove(k);
            self.dirty_since.remove(k);
            self.last_persisted_at.remove(k);
        }
        self.persisted = all
            .into_iter()
            .map(|r| (r.physical_key.clone(), r))
            .collect();
        self.write_atomic()?;
        self.last_flush = Some(now);
        self.writes_since_fsync += 1;
        Ok(FlushOutcome {
            wrote: true,
            fsynced: false,
            evicted,
        })
    }

    /// 原子写：同目录 `.tmp` → write + flush（OS flush）→ rename 就位。
    /// rename 覆盖语义（Windows MOVEFILE_REPLACE_EXISTING）保证旧文件被
    /// 原子替换，崩溃只可能留下孤儿 .tmp（启动时清理）。
    fn write_atomic(&self) -> io::Result<()> {
        let file = LastKnownFile {
            version: SCHEMA_VERSION,
            devices: {
                let mut v: Vec<_> = self.persisted.values().cloned().collect();
                v.sort_by(|a, b| a.physical_key.cmp(&b.physical_key));
                v
            },
        };
        let body = serde_json::to_string_pretty(&file)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        {
            use std::io::Write;
            let mut f = fs::File::create(&tmp)?;
            f.write_all(body.as_bytes())?;
            f.flush()?;
        }
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// fsync 当前文件（5min 定时点 / 退出路径；文件不存在视为无需 fsync）。
    /// 以写权限打开：Windows 下 FlushFileBuffers 要求 GENERIC_WRITE，
    /// 只读句柄 sync_all 会报 Access Denied（Linux 只读 fsync 合法，CI 实测）。
    fn fsync_file(&self) -> io::Result<()> {
        match fs::OpenOptions::new().write(true).open(&self.path) {
            Ok(f) => f.sync_all(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// 启动清理同目录孤儿 `*.tmp`（原子写中途崩溃残留；尽力而为）。
    fn cleanup_orphan_tmps(&self) {
        let Some(dir) = self.path.parent() else {
            return;
        };
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.ends_with(".tmp") {
                let _ = fs::remove_file(e.path());
            }
        }
    }
}

/// display_name 按字符数截断（v3 字段级约束：≤64 字符）。
fn truncate_name(name: &str) -> String {
    name.chars().take(MAX_NAME_CHARS).collect()
}

/// 淘汰检查（条目 ≤32 主机制 + 256KB 保险；同一 LRU 规则：last_seen
/// 最久未见的先淘汰）。返回被淘汰的键。不做时间过期淘汰（v3：旧值
/// 带时间戳展示仍优于「?」）。
fn enforce_caps(records: &mut Vec<PersistedDevice>) -> Vec<String> {
    let mut evicted = Vec::new();
    let evict_oldest = |records: &mut Vec<PersistedDevice>, evicted: &mut Vec<String>| {
        if let Some((idx, _)) = records.iter().enumerate().min_by_key(|(_, r)| r.last_seen) {
            evicted.push(records.remove(idx).physical_key);
        }
    };
    while records.len() > MAX_DEVICES {
        evict_oldest(records, &mut evicted);
    }
    // 256KB 保险：序列化估算（单文件整体），触顶继续同一 LRU 淘汰至回落，
    // 至少保留 1 条（防极端字段膨胀把库清空）。
    while records.len() > 1 && serialized_len(records) > MAX_BYTES {
        evict_oldest(records, &mut evicted);
    }
    evicted
}

fn serialized_len(records: &[PersistedDevice]) -> usize {
    let file = LastKnownFile {
        version: SCHEMA_VERSION,
        devices: records.to_vec(),
    };
    serde_json::to_string(&file).map(|s| s.len()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "volttray-persist-test-{}-{}",
            std::process::id(),
            tag
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fast() -> FlushTuning {
        FlushTuning {
            per_device_debounce: Duration::from_secs(3),
            global_debounce: Duration::from_secs(1),
            flush_fallback: Duration::from_secs(6),
            fsync_interval: Duration::from_secs(10),
        }
    }

    fn rec(key: &str, percent: u8, last_seen: u64) -> PersistedDevice {
        PersistedDevice {
            physical_key: key.into(),
            display_name: format!("Mouse {key}"),
            percent,
            charging: false,
            last_seen,
            source: "unified_battery".into(),
        }
    }

    fn seed_file(dir: &Path, devices: Vec<PersistedDevice>) {
        let file = LastKnownFile {
            version: SCHEMA_VERSION,
            devices,
        };
        fs::write(
            dir.join(STORE_FILE_NAME),
            serde_json::to_string(&file).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn load_missing_file_is_empty() {
        let dir = tmpdir("empty");
        let (_store, outcome) = LastKnownStore::load(dir.join(STORE_FILE_NAME), fast());
        assert_eq!(outcome, LoadOutcome::Empty);
    }

    #[test]
    fn load_round_trip_and_sanitize() {
        let dir = tmpdir("roundtrip");
        let long_name = "N".repeat(100);
        seed_file(
            &dir,
            vec![
                rec("k1", 80, 1000),
                rec("k2", 55, 900),
                // 同键重复：保留 last_seen 最新一条。
                PersistedDevice {
                    percent: 66,
                    last_seen: 1100,
                    ..rec("k2", 55, 900)
                },
                // 空键丢弃；超长名截断。
                rec("", 10, 800),
                PersistedDevice {
                    physical_key: "k3".into(),
                    display_name: long_name.clone(),
                    ..rec("k3", 70, 700)
                },
            ],
        );
        let (store, outcome) = LastKnownStore::load(dir.join(STORE_FILE_NAME), fast());
        assert_eq!(outcome, LoadOutcome::Loaded(3));
        let records = store.records();
        assert_eq!(records.len(), 3);
        let k2 = records.iter().find(|r| r.physical_key == "k2").unwrap();
        assert_eq!((k2.percent, k2.last_seen), (66, 1100));
        let k3 = records.iter().find(|r| r.physical_key == "k3").unwrap();
        assert_eq!(k3.display_name.chars().count(), MAX_NAME_CHARS);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_file_renamed_to_bak_and_recovered() {
        let dir = tmpdir("corrupt");
        fs::write(dir.join(STORE_FILE_NAME), "{not json").unwrap();
        let (_store, outcome) = LastKnownStore::load(dir.join(STORE_FILE_NAME), fast());
        assert_eq!(outcome, LoadOutcome::Recovered);
        // 原文件已改名 .bak（证据保留），主文件不存在。
        assert!(!dir.join(STORE_FILE_NAME).exists());
        assert_eq!(
            fs::read_to_string(dir.join(STORE_BACKUP_NAME)).unwrap(),
            "{not json"
        );
        // version 不支持同样按损坏处理。
        seed_file(&dir, vec![]);
        let raw = fs::read_to_string(dir.join(STORE_FILE_NAME))
            .unwrap()
            .replace("\"version\":1", "\"version\":99");
        fs::write(dir.join(STORE_FILE_NAME), raw).unwrap();
        let (_s2, outcome2) = LastKnownStore::load(dir.join(STORE_FILE_NAME), fast());
        assert_eq!(outcome2, LoadOutcome::Recovered);
        // .bak 被替换（文件数恒 ≤2：主文件缺席 + 唯一 .bak）。
        let files: Vec<_> = fs::read_dir(&dir).unwrap().flatten().collect();
        assert_eq!(files.len(), 1);
        assert!(dir.join(STORE_BACKUP_NAME).exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn orphan_tmp_cleaned_and_valid_main_survives() {
        let dir = tmpdir("tmp-clean");
        seed_file(&dir, vec![rec("k1", 80, 1000)]);
        // 模拟原子写中途崩溃：孤儿 .tmp 与主文件并存。
        fs::write(dir.join("last_known.json.tmp"), "partial...").unwrap();
        let (store, outcome) = LastKnownStore::load(dir.join(STORE_FILE_NAME), fast());
        assert_eq!(outcome, LoadOutcome::Loaded(1));
        assert!(!dir.join("last_known.json.tmp").exists());
        assert_eq!(store.records()[0].percent, 80);
        // 只有孤儿 .tmp（主文件未 rename 就位）→ 清理后 Empty，不误判损坏。
        let dir2 = tmpdir("tmp-only");
        fs::write(dir2.join("last_known.json.tmp"), "partial...").unwrap();
        let (_s, outcome2) = LastKnownStore::load(dir2.join(STORE_FILE_NAME), fast());
        assert_eq!(outcome2, LoadOutcome::Empty);
        assert!(!dir2.join("last_known.json.tmp").exists());
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&dir2);
    }

    #[test]
    fn change_driven_dirty_and_value_refresh() {
        let dir = tmpdir("dirty");
        let (mut store, _) = LastKnownStore::load(dir.join(STORE_FILE_NAME), fast());
        let t0 = Instant::now();
        // 新键 → dirty。
        assert!(store.note_reading("k1", "M1", 80, false, "unified_battery", 1000, t0));
        // 同值 → 不 dirty（last_seen 仍刷新，LRU 保鲜）。
        assert!(!store.note_reading("k1", "M1", 80, false, "unified_battery", 1001, t0));
        assert_eq!(store.records()[0].last_seen, 1001);
        // 充电态变化 → dirty；百分比变化 → dirty。
        assert!(store.note_reading("k1", "M1", 80, true, "unified_battery", 1002, t0));
        assert!(store.note_reading("k1", "M1", 81, true, "unified_battery", 1003, t0));
        // 空键 / 冲突键永不入库。
        assert!(!store.note_reading("", "M?", 1, false, "s", 1004, t0));
        let mut conflicted = HashSet::new();
        conflicted.insert("kc".to_string());
        store.set_conflicted(conflicted);
        assert!(!store.note_reading("kc", "MC", 50, false, "s", 1005, t0));
        assert!(store.records().iter().all(|r| r.physical_key != "kc"));
        let _ = fs::remove_dir_all(&dir);
    }

    fn on_disk_percent(dir: &Path, key: &str) -> Option<u64> {
        let raw = fs::read_to_string(dir.join(STORE_FILE_NAME)).ok()?;
        let f: serde_json::Value = serde_json::from_str(&raw).unwrap();
        f["devices"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["physical_key"] == key)
            .map(|d| d["percent"].as_u64().unwrap())
    }

    #[test]
    fn debounce_per_device_and_global() {
        let dir = tmpdir("debounce");
        // per_device=3s，global=1s，fallback=6s，fsync=10s。
        let (mut store, _) = LastKnownStore::load(dir.join(STORE_FILE_NAME), fast());
        let t0 = Instant::now();
        let note = |s: &mut LastKnownStore, k: &str, p: u8, t: Instant| {
            s.note_reading(k, "M", p, false, "unified_battery", 1000, t)
        };
        // 首次落盘不受全局防抖限制（从未写过）→ 立即写。
        note(&mut store, "k1", 80, t0);
        let out = store.flush_if_due(t0).unwrap();
        assert!(out.wrote);
        assert_eq!(on_disk_percent(&dir, "k1"), Some(80));
        // 全局窗口（1s）内不落盘。
        note(&mut store, "k1", 81, t0 + Duration::from_millis(500));
        let out = store.flush_if_due(t0 + Duration::from_millis(500)).unwrap();
        assert!(!out.wrote);
        // 全局满足但 k1 的 per-device（3s）未满 → 仍不写。
        let out = store.flush_if_due(t0 + Duration::from_secs(2)).unwrap();
        assert!(!out.wrote);
        assert_eq!(on_disk_percent(&dir, "k1"), Some(80));
        // k2 新键（从未持久化）在全局满足后立即可写；k1 继续等 per-device。
        note(&mut store, "k2", 60, t0 + Duration::from_secs(2));
        let out = store.flush_if_due(t0 + Duration::from_secs(2)).unwrap();
        assert!(out.wrote);
        assert_eq!(on_disk_percent(&dir, "k2"), Some(60));
        assert_eq!(
            on_disk_percent(&dir, "k1"),
            Some(80),
            "k1 仍在 per-device 窗口"
        );
        // k1 距上次持久化满 3s → 写入。
        let out = store.flush_if_due(t0 + Duration::from_secs(4)).unwrap();
        assert!(out.wrote);
        assert_eq!(on_disk_percent(&dir, "k1"), Some(81));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn flush_fallback_overrides_per_device_window() {
        let dir = tmpdir("fallback");
        // per_device 长窗口（30s）+ 短兜底（6s）：验证「dirty 后 60s 内
        // OS flush 兜底一次」优先于 per-device 防抖。
        let tuning = FlushTuning {
            per_device_debounce: Duration::from_secs(30),
            global_debounce: Duration::from_secs(1),
            flush_fallback: Duration::from_secs(6),
            fsync_interval: Duration::from_secs(300),
        };
        let (mut store, _) = LastKnownStore::load(dir.join(STORE_FILE_NAME), tuning);
        let t0 = Instant::now();
        let note = |s: &mut LastKnownStore, p: u8, t: Instant| {
            s.note_reading("k1", "M", p, false, "unified_battery", 1000, t)
        };
        note(&mut store, 80, t0);
        assert!(store.flush_if_due(t0).unwrap().wrote);
        // t0+1 变 dirty：per-device 远未满、兜底未满 → 不写。
        note(&mut store, 81, t0 + Duration::from_secs(1));
        let out = store.flush_if_due(t0 + Duration::from_secs(2)).unwrap();
        assert!(!out.wrote);
        assert_eq!(on_disk_percent(&dir, "k1"), Some(80));
        // 兜底：dirty 满 6s → per-device 未满也强制落盘。
        let out = store.flush_if_due(t0 + Duration::from_secs(8)).unwrap();
        assert!(out.wrote);
        assert_eq!(
            on_disk_percent(&dir, "k1"),
            Some(81),
            "60s 兜底应压过 per-device 窗口"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn fsync_gated_by_interval_and_shutdown_forces() {
        let dir = tmpdir("fsync");
        let (mut store, _) = LastKnownStore::load(dir.join(STORE_FILE_NAME), fast());
        let t0 = Instant::now();
        store.note_reading("k1", "M", 80, false, "unified_battery", 1000, t0);
        // 首次落盘：writes_since_fsync 0→1，同 tick fsync（last_fsync 从未有过）。
        let out = store.flush_if_due(t0 + Duration::from_secs(1)).unwrap();
        assert!(out.wrote && out.fsynced);
        // 间隔内再次落盘：不 fsync。
        store.note_reading(
            "k1",
            "M",
            81,
            false,
            "unified_battery",
            1001,
            t0 + Duration::from_secs(2),
        );
        let out = store.flush_if_due(t0 + Duration::from_secs(5)).unwrap();
        assert!(out.wrote && !out.fsynced);
        // 过 fsync 间隔（10s）后 → fsync。
        store.note_reading(
            "k1",
            "M",
            82,
            false,
            "unified_battery",
            1002,
            t0 + Duration::from_secs(9),
        );
        let out = store.flush_if_due(t0 + Duration::from_secs(12)).unwrap();
        assert!(out.wrote && out.fsynced);
        // 退出：强制落盘全部 dirty + fsync（无视防抖窗口）。
        store.note_reading(
            "k1",
            "M",
            83,
            false,
            "unified_battery",
            1003,
            t0 + Duration::from_secs(13),
        );
        let out = store.shutdown_flush(t0 + Duration::from_secs(13)).unwrap();
        assert!(out.wrote && out.fsynced);
        let f: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(dir.join(STORE_FILE_NAME)).unwrap()).unwrap();
        assert_eq!(f["devices"][0]["percent"].as_u64().unwrap(), 83);
        assert_eq!(f["version"].as_u64().unwrap(), SCHEMA_VERSION as u64);
        // 无 dirty 的退出也保证 fsync 语义且不写盘。
        let out = store.shutdown_flush(t0 + Duration::from_secs(14)).unwrap();
        assert!(!out.wrote && out.fsynced);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn lru_eviction_at_load_and_flush() {
        let dir = tmpdir("lru");
        // 启动加载淘汰：34 条 → 淘汰 last_seen 最旧的 2 条。
        let devices: Vec<_> = (0..34)
            .map(|i| rec(&format!("k{i:02}"), 50, 1000 + i as u64))
            .collect();
        seed_file(&dir, devices);
        let (mut store, outcome) = LastKnownStore::load(dir.join(STORE_FILE_NAME), fast());
        assert_eq!(outcome, LoadOutcome::Loaded(MAX_DEVICES));
        let keys: Vec<_> = store
            .records()
            .into_iter()
            .map(|r| r.physical_key)
            .collect();
        assert!(!keys.contains(&"k00".to_string()));
        assert!(!keys.contains(&"k01".to_string()));
        assert!(keys.contains(&"k33".to_string()));
        // 落盘淘汰：写满 32 条后再来一条 → 最旧的 k02 被淘汰并随文件落盘。
        let t0 = Instant::now();
        store.note_reading("k-new", "MN", 90, false, "unified_battery", 2000, t0);
        let out = store.flush_if_due(t0).unwrap();
        assert!(out.wrote);
        assert_eq!(out.evicted, vec!["k02".to_string()]);
        assert!(store.records().iter().all(|r| r.physical_key != "k02"));
        let on_disk = fs::read_to_string(dir.join(STORE_FILE_NAME)).unwrap();
        assert!(!on_disk.contains("k02"));
        assert!(on_disk.contains("k-new"));
        let f: serde_json::Value = serde_json::from_str(&on_disk).unwrap();
        assert_eq!(f["devices"].as_array().unwrap().len(), MAX_DEVICES);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn byte_cap_insurance_evicts_to_limit() {
        let dir = tmpdir("bytes");
        // 每条记录约 8KB（超长名 64 字符不够，手工构造膨胀 source 字段
        // 模拟「字段异常膨胀」），33 条即超 256KB → 保险淘汰至回落。
        let big = "S".repeat(8 * 1024);
        let devices: Vec<_> = (0..33)
            .map(|i| PersistedDevice {
                source: big.clone(),
                ..rec(&format!("k{i:02}"), 50, 1000 + i as u64)
            })
            .collect();
        seed_file(&dir, devices);
        let (store, outcome) = LastKnownStore::load(dir.join(STORE_FILE_NAME), fast());
        let n = match outcome {
            LoadOutcome::Loaded(n) => n,
            other => panic!("expected Loaded, got {other:?}"),
        };
        assert!((1..33).contains(&n), "256KB 保险应淘汰至回落，实际 {n}");
        let size = serialized_len(&store.records());
        assert!(size <= MAX_BYTES, "回落后仍超限：{size}");
        // 保留的是 last_seen 最新的一批。
        assert!(store.records().iter().any(|r| r.physical_key == "k32"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn conflict_detection_stops_writing_and_drops_existing_records() {
        let dir = tmpdir("conflict");
        seed_file(&dir, vec![rec("shared-name", 70, 1000)]);
        let (mut store, outcome) = LastKnownStore::load(dir.join(STORE_FILE_NAME), fast());
        assert_eq!(outcome, LoadOutcome::Loaded(1));
        let t0 = Instant::now();
        // 冲突出现：停写 + 存量记录即时清除（红队复核建议口径）。
        let mut keys = HashSet::new();
        keys.insert("shared-name".to_string());
        let removed = store.set_conflicted(keys);
        assert_eq!(removed, vec!["shared-name".to_string()]);
        assert!(store.records().is_empty());
        // 冲突键读数永不入库。
        assert!(!store.note_reading("shared-name", "M", 71, false, "s", 1001, t0));
        // 冲突解除后恢复可写。
        let removed2 = store.set_conflicted(HashSet::new());
        assert!(removed2.is_empty());
        assert!(store.note_reading("shared-name", "M", 71, false, "s", 1002, t0));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn reload_after_flush_sees_persisted_values() {
        let dir = tmpdir("reload");
        let (mut store, _) = LastKnownStore::load(dir.join(STORE_FILE_NAME), fast());
        let t0 = Instant::now();
        store.note_reading(
            "k1",
            "MX Master",
            62,
            true,
            "battery_status",
            1_700_000_000,
            t0,
        );
        let out = store.flush_if_due(t0 + Duration::from_secs(1)).unwrap();
        assert!(out.wrote);
        let (store2, outcome) = LastKnownStore::load(dir.join(STORE_FILE_NAME), fast());
        assert_eq!(outcome, LoadOutcome::Loaded(1));
        let r = &store2.records()[0];
        assert_eq!(r.physical_key, "k1");
        assert_eq!(r.display_name, "MX Master");
        assert_eq!((r.percent, r.charging), (62, true));
        assert_eq!(r.last_seen, 1_700_000_000);
        assert_eq!(r.source, "battery_status");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn simulated_30d_write_storm_stays_bounded() {
        // AC-A5 等效模拟的核心不变量：按声明上限速率连续注入变化事件，
        // 文件数 ≤2（含 .bak）、大小 ≤256KB、LRU 淘汰正确。
        // （30 天事件量 = 全局 10s 一次 ≈ 259k 次落盘，单测按同一代码路径
        // 以加速节拍跑 3k 次注入 + 300 次落盘验证不变量；满量由验收脚本执行。）
        let dir = tmpdir("storm");
        let (mut store, _) = LastKnownStore::load(dir.join(STORE_FILE_NAME), fast());
        let t0 = Instant::now();
        for round in 0..300u64 {
            for d in 0..10u64 {
                let key = format!("dev-{}", (round * 10 + d) % 40); // 40 台轮换 > 32 上限
                store.note_reading(
                    &key,
                    &format!("Storm Mouse {}", (round * 10 + d) % 40),
                    (round % 100) as u8,
                    round % 2 == 0,
                    "unified_battery",
                    1_000_000 + round * 10 + d,
                    t0 + Duration::from_secs(round * 7),
                );
            }
            let out = store
                .flush_if_due(t0 + Duration::from_secs(round * 7 + 6))
                .unwrap();
            let files: Vec<_> = fs::read_dir(&dir).unwrap().flatten().collect();
            assert!(files.len() <= 2, "文件数超过 2：{}", files.len());
            if out.wrote {
                let size = fs::metadata(dir.join(STORE_FILE_NAME)).unwrap().len() as usize;
                assert!(size <= MAX_BYTES, "文件超限：{size}");
                let f: serde_json::Value =
                    serde_json::from_str(&fs::read_to_string(dir.join(STORE_FILE_NAME)).unwrap())
                        .unwrap();
                assert!(f["devices"].as_array().unwrap().len() <= MAX_DEVICES);
            }
        }
        // LRU：40 台轮换，最后 300 轮注入的 10 台必在库中。
        let keys: Vec<String> = store
            .records()
            .into_iter()
            .map(|r| r.physical_key)
            .collect();
        for d in 0..10u64 {
            let key = format!("dev-{}", (299 * 10 + d) % 40);
            assert!(keys.contains(&key), "最近活跃设备被误淘汰：{key}");
        }
        let _ = fs::remove_dir_all(&dir);
    }
}
