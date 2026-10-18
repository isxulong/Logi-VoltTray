//! volttray-app —— WP-B 耗电统计：stats.jsonl 事件落盘与读取。
//!
//! 口径来源（v3 设计文档 WP-B 节，唯一基线）：
//! - append-only `stats.jsonl`，每行一事件；封顶 10MB，触顶轮转：
//!   现文件 rename 为 `stats.archive.jsonl`（覆盖旧归档）→ 开新文件续写，
//!   文件数恒 ≤2（双文件 ≤20MB）。
//! - 事件 schema 统一定义（二轮红队 P2-1）：① 读取 `{ts, physical_key,
//!   display_name, percent, charging, source}`（source ∈ poll /
//!   charging-fast / wake-first-read）；② `{ts, event:"run_start"|"run_stop"}`；
//!   ③ `{ts, event:"boundary", devices:[{physical_key, percent, charging}]}`；
//!   ④ `{ts, physical_key, event:"sleep_enter"}`。
//! - ts 为 epoch 秒整数（D-75：v3 未钉 jsonl 内 ts 编码；整数解析最稳健，
//!   RFC3339 本地偏移呈现只发生在 CSV/HTML 导出渲染层）。
//! - 变化驱动采集：percent/charging 变化、或本会话该键首条（新键锚点）、
//!   或 wake-first-read 才落事件；回退键冲突设备不记录（R5-I-01 对齐）。
//! - 落盘节拍复用 WP-A 落盘协调器参数集（FlushTuning）：全局 10s 防抖、
//!   60s OS flush 兜底、5min fsync 定时点 + 退出 fsync——per-device 30s
//!   防抖对 append-only 事件流不适用，其限流角色由变化驱动采集承担
//!   （D-75）；丢失口径与 A-1 一致：崩溃最坏丢 60s、掉电最坏丢 5 分钟。
//! - 半截行：启动校验尾部行完整性，最后一行不完整则截断至最后完整行；
//!   读取端（导出/归因/复算）统一 skip-bad-lines 作二道防线。
//! - 跨轮转闭合：轮转时在新文件头部写一条 boundary 事件（携带轮转时刻
//!   各设备最后读数）；归因与导出同读 current + archive。
//! - 轮转护栏：覆盖旧归档时置提示信号位，GUI 据此 toast「统计历史即将
//!   轮转，建议先导出」（toast 不可点击，一键导出走菜单导出项）。
//!
//! 红线：本模块为主机侧记账，全部事件来自既有读数收敛点与既有超时判定，
//! 不新增任何设备查询。

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::persist::{FlushTuning, MAX_NAME_CHARS, STORE_DIR_NAME};

/// 统计主文件名（current）。
pub const STATS_FILE_NAME: &str = "stats.jsonl";
/// 轮转归档名（覆盖式，文件数恒 ≤2）。
pub const STATS_ARCHIVE_NAME: &str = "stats.archive.jsonl";
/// 单文件大小封顶 10MB（触顶轮转；双文件合计 ≤20MB）。
pub const STATS_MAX_BYTES: u64 = 10 * 1024 * 1024;
/// 轮转水位预提示阈值（90%）：预计落盘后越过该水位但未触顶时置
/// 轮转提示信号，GUI toast「统计历史即将轮转，建议先导出」——v3 口径
/// 「覆盖旧归档前弹一次 toast」以此提前一拍实现（质检 F2：旧实现先
/// 覆盖后弹，信号发出时旧归档已不可导出，与时序口径倒置）。
pub const STATS_ROTATE_WARN_BYTES: u64 = STATS_MAX_BYTES * 9 / 10;

/// 读取事件 source 枚举（v3 钉死三个取值）。
pub const SOURCE_POLL: &str = "poll";
pub const SOURCE_CHARGING_FAST: &str = "charging-fast";
pub const SOURCE_WAKE_FIRST_READ: &str = "wake-first-read";

/// 读取事件的 source 判定（v3 WP-C per-device 节拍语义）：读取前设备
/// 处于休眠态（本次为唤醒后首读）→ wake-first-read；否则读数 charging
/// =true（充电档 15s 节奏）→ charging-fast；其余 → poll。分类按读取
/// 发生时的节拍，与归因引擎的段类型判定互不依赖。
pub fn read_source(was_sleeping: bool, charging: bool) -> &'static str {
    if was_sleeping {
        SOURCE_WAKE_FIRST_READ
    } else if charging {
        SOURCE_CHARGING_FAST
    } else {
        SOURCE_POLL
    }
}

/// 事件类型判别字段值。
const EVENT_RUN_START: &str = "run_start";
const EVENT_RUN_STOP: &str = "run_stop";
const EVENT_BOUNDARY: &str = "boundary";
const EVENT_SLEEP_ENTER: &str = "sleep_enter";

/// 统计目录：`%LOCALAPPDATA%\LogiVoltTray\`（与 last_known.json 同目录，
/// 无 LOCALAPPDATA 时退回当前目录，与 persist::default_path 同款兜底）。
pub fn default_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join(STORE_DIR_NAME)
}

/// 默认统计主文件路径。
pub fn default_path() -> PathBuf {
    default_dir().join(STATS_FILE_NAME)
}

/// 读取事件（schema ①）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadEvent {
    pub ts: i64,
    pub physical_key: String,
    pub display_name: String,
    pub percent: u8,
    pub charging: bool,
    pub source: String,
}

/// boundary 事件携带的单设备读数（schema ③子结构）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundaryDevice {
    pub physical_key: String,
    pub percent: u8,
    pub charging: bool,
}

/// stats.jsonl 事件（四类型统一；序列化按 v3 schema 逐类型定制）。
#[derive(Debug, Clone, PartialEq)]
pub enum StatsEvent {
    Read(ReadEvent),
    RunStart {
        ts: i64,
    },
    RunStop {
        ts: i64,
    },
    Boundary {
        ts: i64,
        devices: Vec<BoundaryDevice>,
    },
    SleepEnter {
        ts: i64,
        physical_key: String,
    },
}

impl StatsEvent {
    /// 事件时间戳（epoch 秒）。
    pub fn ts(&self) -> i64 {
        match self {
            StatsEvent::Read(e) => e.ts,
            StatsEvent::RunStart { ts }
            | StatsEvent::RunStop { ts }
            | StatsEvent::Boundary { ts, .. }
            | StatsEvent::SleepEnter { ts, .. } => *ts,
        }
    }

    /// 事件涉及的设备键（run/boundary 全局事件语义见各变体）。
    pub fn physical_key(&self) -> Option<&str> {
        match self {
            StatsEvent::Read(e) => Some(&e.physical_key),
            StatsEvent::SleepEnter { physical_key, .. } => Some(physical_key),
            _ => None,
        }
    }

    /// 序列化为单行 JSON（v3 事件 schema 逐类型定制字段集）。
    pub fn to_json_line(&self) -> String {
        match self {
            StatsEvent::Read(e) => serde_json::json!({
                "ts": e.ts,
                "physical_key": e.physical_key,
                "display_name": e.display_name,
                "percent": e.percent,
                "charging": e.charging,
                "source": e.source,
            })
            .to_string(),
            StatsEvent::RunStart { ts } => {
                serde_json::json!({"ts": ts, "event": EVENT_RUN_START}).to_string()
            }
            StatsEvent::RunStop { ts } => {
                serde_json::json!({"ts": ts, "event": EVENT_RUN_STOP}).to_string()
            }
            StatsEvent::Boundary { ts, devices } => serde_json::json!({
                "ts": ts,
                "event": EVENT_BOUNDARY,
                "devices": devices,
            })
            .to_string(),
            StatsEvent::SleepEnter { ts, physical_key } => serde_json::json!({
                "ts": ts,
                "physical_key": physical_key,
                "event": EVENT_SLEEP_ENTER,
            })
            .to_string(),
        }
    }

    /// 解析单行 JSON。**skip-bad-lines 按各事件类型自己的必填字段校验**
    /// （v3 红队 P2-1：不得拿读取事件的字段集把 ②③④ 当坏行误杀）；
    /// 容忍额外字段（前向兼容），校验失败返回 None。
    pub fn parse_line(line: &str) -> Option<StatsEvent> {
        let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
        let obj = v.as_object()?;
        let ts = obj.get("ts")?.as_i64()?;
        match obj.get("event").and_then(|e| e.as_str()) {
            None => {
                // 读取事件：ts + physical_key + display_name + percent(0..=100)
                // + charging + source（三枚举值之一）。
                let physical_key = obj.get("physical_key")?.as_str()?.to_string();
                let display_name = obj.get("display_name")?.as_str()?.to_string();
                let percent = obj.get("percent")?.as_u64()?;
                let percent = u8::try_from(percent).ok().filter(|p| *p <= 100)?;
                let charging = obj.get("charging")?.as_bool()?;
                let source = obj.get("source")?.as_str()?;
                if !matches!(
                    source,
                    SOURCE_POLL | SOURCE_CHARGING_FAST | SOURCE_WAKE_FIRST_READ
                ) {
                    return None;
                }
                Some(StatsEvent::Read(ReadEvent {
                    ts,
                    physical_key,
                    display_name,
                    percent,
                    charging,
                    source: source.to_string(),
                }))
            }
            Some(EVENT_RUN_START) => Some(StatsEvent::RunStart { ts }),
            Some(EVENT_RUN_STOP) => Some(StatsEvent::RunStop { ts }),
            Some(EVENT_BOUNDARY) => {
                let devices: Vec<BoundaryDevice> =
                    serde_json::from_value(obj.get("devices")?.clone()).ok()?;
                Some(StatsEvent::Boundary { ts, devices })
            }
            Some(EVENT_SLEEP_ENTER) => {
                let physical_key = obj.get("physical_key")?.as_str()?.to_string();
                Some(StatsEvent::SleepEnter { ts, physical_key })
            }
            // 未知事件类型：坏行跳过（前向兼容的保守口径——不认识即不采信）。
            Some(_) => None,
        }
    }
}

/// display_name 按字符数截断（v3 字段级约束 ≤64 字符，与 A-1 同口径；
/// 单事件约 170B 的容量估算依赖此截断）。
fn truncate_name(name: &str) -> String {
    name.chars().take(MAX_NAME_CHARS).collect()
}

/// 启动加载结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatsLoadOutcome {
    /// 主文件不存在（首次运行）——空文件起步。
    Empty,
    /// 主文件就绪（尾部完整）。
    Ready,
    /// 尾部半截行已截断修复（附带截掉的字节数；崩溃中途写盘残留）。
    Repaired(u64),
}

/// 一次落盘尝试的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StatsFlushOutcome {
    /// 本次是否实际写盘。
    pub wrote: bool,
    /// 本次是否执行了 fsync。
    pub fsynced: bool,
    /// 本次是否发生了轮转（GUI 据此提示用户先导出）。
    pub rotated: bool,
}

/// stats.jsonl 落盘器（append-only、10MB 轮转、防抖批量落盘）。
///
/// 时间注入：`ts`（epoch 秒，事件与 boundary 时间戳）与 `now`（Instant，
/// 防抖/fsync 节拍）全部由调用方给出，跨平台可测。
#[derive(Debug)]
pub struct StatsStore {
    path: PathBuf,
    archive_path: PathBuf,
    tuning: FlushTuning,
    /// 待落盘事件行（已序列化，不含换行符）。
    buf: Vec<String>,
    /// buf 累计字节数（含每行换行符），轮转预判用。
    buf_bytes: u64,
    /// 首次有缓冲事件等待落盘的时刻（60s 兜底判定）。
    dirty_since: Option<Instant>,
    /// 最近一次落盘时刻（10s 全局防抖）。
    last_flush: Option<Instant>,
    /// 距上次 fsync 以来的落盘次数。
    writes_since_fsync: u32,
    /// 最近一次 fsync 时刻（5min 定时）。
    last_fsync: Option<Instant>,
    /// 变化驱动过滤：本会话各键最近一次已记录（落 buf）的读数。
    last_recorded: HashMap<String, (u8, bool)>,
    /// 回退键冲突集合（R5-I-01 对齐：冲突设备不记录统计）。
    conflicted: HashSet<String>,
    /// 轮转护栏信号位：预计越过水位（≥9MB）而未触顶时置位，GUI 取走
    /// 后 toast（v3「覆盖旧归档前提示」的提前一拍实现，质检 F2）。
    rotation_notice: bool,
    /// 水位提示闩锁：本轮 current 文件生命周期内只提示一次（轮转后
    /// 复位，避免每次落盘重复 toast）。
    rotation_warned: bool,
}

impl StatsStore {
    /// 生产构造（默认节拍；dir 为统计目录）。
    pub fn new(dir: &Path) -> Self {
        Self::with_tuning(dir, FlushTuning::default())
    }

    /// 测试构造（注入节拍）。
    pub fn with_tuning(dir: &Path, tuning: FlushTuning) -> Self {
        StatsStore {
            path: dir.join(STATS_FILE_NAME),
            archive_path: dir.join(STATS_ARCHIVE_NAME),
            tuning,
            buf: Vec::new(),
            buf_bytes: 0,
            dirty_since: None,
            last_flush: None,
            writes_since_fsync: 0,
            last_fsync: None,
            last_recorded: HashMap::new(),
            conflicted: HashSet::new(),
            rotation_notice: false,
            rotation_warned: false,
        }
    }

    /// 启动加载：清理同目录孤儿 `.tmp`（WP-A 同款兜底），校验主文件
    /// 尾部行完整性——最后一行不完整则截断至最后完整行（红队 P1 修订：
    /// 崩溃中途写盘只丢最后一行）。
    pub fn load(dir: &Path, tuning: FlushTuning) -> (Self, StatsLoadOutcome) {
        cleanup_orphan_tmps(dir);
        let store = Self::with_tuning(dir, tuning);
        let outcome = match store.repair_tail() {
            Ok(o) => o,
            Err(_) => StatsLoadOutcome::Empty, // 不可读按空起步，不阻塞启动
        };
        (store, outcome)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn archive_path(&self) -> &Path {
        &self.archive_path
    }

    /// 同步回退键冲突集合（冲突键不记录统计；调用方每轮刷新）。
    pub fn set_conflicted(&mut self, keys: HashSet<String>) {
        self.conflicted = keys;
    }

    /// 取走轮转提示信号（GUI 每次 pump 轮询一次；true → toast 一次）。
    pub fn take_rotation_notice(&mut self) -> bool {
        std::mem::take(&mut self.rotation_notice)
    }

    /// 采集钩子（变化驱动的唯一收敛点）：成功读数后由 IO 循环调用。
    /// 返回是否新记录了事件。过滤规则（v3）：percent/charging 任一变化、
    /// 或本会话该键首条（新键锚点——跨 run 分段需要 run 内锚点，代价为
    /// 每 run 每设备至多 1 行，D-75）、或 wake-first-read（休眠段终点
    /// 锚点，必记）才落事件；冲突键不记录。
    #[allow(clippy::too_many_arguments)]
    pub fn note_reading(
        &mut self,
        physical_key: &str,
        display_name: &str,
        percent: u8,
        charging: bool,
        source: &str,
        ts: i64,
        now: Instant,
    ) -> bool {
        if self.conflicted.contains(physical_key) {
            return false;
        }
        let changed = match self.last_recorded.get(physical_key) {
            None => true, // 本会话首条 = 新键锚点
            Some(&(p, c)) => p != percent || c != charging,
        };
        if !changed && source != SOURCE_WAKE_FIRST_READ {
            return false;
        }
        self.last_recorded
            .insert(physical_key.to_string(), (percent, charging));
        self.push_event(
            StatsEvent::Read(ReadEvent {
                ts,
                physical_key: physical_key.to_string(),
                display_name: truncate_name(display_name),
                percent,
                charging,
                source: source.to_string(),
            }),
            now,
        );
        true
    }

    /// run_start 生命周期事件（每次运行 2 行之一）。
    pub fn note_run_start(&mut self, ts: i64, now: Instant) {
        self.push_event(StatsEvent::RunStart { ts }, now);
    }

    /// run_stop 生命周期事件（正常退出 / WM_ENDSESSION 承接路径）。
    pub fn note_run_stop(&mut self, ts: i64, now: Instant) {
        self.push_event(StatsEvent::RunStop { ts }, now);
    }

    /// sleep_enter 事件（2 次超时判定休眠时落盘；判定本就发生，仅多
    /// 落一行日志，零新增设备查询——红队 P0-1 必修）。
    pub fn note_sleep_enter(&mut self, physical_key: &str, ts: i64, now: Instant) {
        if self.conflicted.contains(physical_key) {
            return;
        }
        self.push_event(
            StatsEvent::SleepEnter {
                ts,
                physical_key: physical_key.to_string(),
            },
            now,
        );
    }

    /// 缓冲一行事件并标 dirty。
    fn push_event(&mut self, ev: StatsEvent, now: Instant) {
        let line = ev.to_json_line();
        self.buf_bytes += line.len() as u64 + 1; // +1 换行符
        self.buf.push(line);
        self.dirty_since.get_or_insert(now);
    }

    /// 周期落盘点：全局 10s 防抖 + dirty 后 60s OS flush 兜底 + 5min
    /// fsync 定时（节拍参数整体继承 WP-A FlushTuning）。
    pub fn flush_if_due(&mut self, now: Instant, ts: i64) -> io::Result<StatsFlushOutcome> {
        let mut outcome = StatsFlushOutcome::default();
        if !self.buf.is_empty() {
            let global_ok = self
                .last_flush
                .is_none_or(|t| now.duration_since(t) >= self.tuning.global_debounce);
            let fallback = self
                .dirty_since
                .is_some_and(|t| now.duration_since(t) >= self.tuning.flush_fallback);
            if global_ok || fallback {
                outcome = self.append_buffered(now, ts)?;
            }
        }
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

    /// 退出路径（含 WM_ENDSESSION 承接）：强制落盘全部缓冲 + fsync。
    pub fn shutdown_flush(&mut self, now: Instant, ts: i64) -> io::Result<StatsFlushOutcome> {
        let mut outcome = if self.buf.is_empty() {
            StatsFlushOutcome::default()
        } else {
            self.append_buffered(now, ts)?
        };
        if self.writes_since_fsync > 0 {
            self.fsync_file()?;
            self.last_fsync = Some(now);
            self.writes_since_fsync = 0;
        }
        outcome.fsynced = true;
        Ok(outcome)
    }

    /// 实际落盘：轮转预判（现文件 + 缓冲 > 10MB → 先轮转）→ append；
    /// 未触顶但预计越过水位（≥9MB）时置轮转提示信号（每轮 current 仅
    /// 一次，闩锁防抖；单次 flush 从水位下直接跳窗触顶的场景不补弹——
    /// 轮转已发生后再说「即将轮转」为伪陈述，质检 F2 口径）。
    fn append_buffered(&mut self, now: Instant, ts: i64) -> io::Result<StatsFlushOutcome> {
        let mut rotated = false;
        let cur_len = fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        let projected = cur_len + self.buf_bytes;
        if cur_len > 0 && projected > STATS_MAX_BYTES {
            self.rotate(ts)?;
            rotated = true;
        } else if cur_len > 0 && !self.rotation_warned && projected >= STATS_ROTATE_WARN_BYTES {
            // 水位预提示：覆盖旧归档前约 1MB 余量处提前 toast，用户可在
            // 旧归档仍完好时一键导出（v3 护栏时序，质检 F2）。
            self.rotation_notice = true;
            self.rotation_warned = true;
        }
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        {
            let mut f = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
            for line in &self.buf {
                f.write_all(line.as_bytes())?;
                f.write_all(b"\n")?;
            }
            f.flush()?;
        }
        self.buf.clear();
        self.buf_bytes = 0;
        self.dirty_since = None;
        self.last_flush = Some(now);
        self.writes_since_fsync += 1;
        Ok(StatsFlushOutcome {
            wrote: true,
            fsynced: false,
            rotated,
        })
    }

    /// 触顶轮转：现文件 rename 为归档（覆盖旧归档）→ 新文件头部写
    /// boundary 事件（携带轮转时刻各设备最后读数，闭合跨轮转的段）。
    /// 轮转提示信号不在此置位——护栏 toast 由 append_buffered 的水位
    /// 预提示在覆盖旧归档之前发出（质检 F2 时序口径）；此处仅复位
    /// 水位闩锁，新 current 文件生命周期重新允许一次预提示。
    fn rotate(&mut self, ts: i64) -> io::Result<()> {
        // boundary 载荷：现文件各设备最后读数 ∪ 本会话缓冲内最新读数
        //（缓冲尚未落盘的部分以会话内记录为准）。
        let mut last: HashMap<String, (u8, bool)> = HashMap::new();
        if let Ok(body) = fs::read_to_string(&self.path) {
            for line in body.lines() {
                if let Some(StatsEvent::Read(e)) = StatsEvent::parse_line(line) {
                    last.insert(e.physical_key, (e.percent, e.charging));
                }
            }
        }
        for (k, v) in &self.last_recorded {
            last.insert(k.clone(), *v);
        }
        if self.path.exists() {
            fs::rename(&self.path, &self.archive_path)?;
        }
        self.rotation_warned = false;
        let mut devices: Vec<BoundaryDevice> = last
            .into_iter()
            .map(|(physical_key, (percent, charging))| BoundaryDevice {
                physical_key,
                percent,
                charging,
            })
            .collect();
        devices.sort_by(|a, b| a.physical_key.cmp(&b.physical_key));
        let head = StatsEvent::Boundary { ts, devices }.to_json_line();
        let mut f = fs::File::create(&self.path)?;
        f.write_all(head.as_bytes())?;
        f.write_all(b"\n")?;
        f.flush()?;
        Ok(())
    }

    /// fsync 当前文件（5min 定时点 / 退出路径；文件不存在视为无需 fsync）。
    fn fsync_file(&self) -> io::Result<()> {
        match fs::File::open(&self.path) {
            Ok(f) => f.sync_all(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// 尾部半截行校验与截断（启动一道防线）。
    fn repair_tail(&self) -> io::Result<StatsLoadOutcome> {
        let body = match fs::read(&self.path) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(StatsLoadOutcome::Empty),
            Err(e) => return Err(e),
        };
        if body.is_empty() {
            return Ok(StatsLoadOutcome::Ready);
        }
        // 最后一行完整 = 文件以换行结尾，或末行本身是合法事件行
        //（历史写入均以 \n 结尾；不以 \n 结尾且不可解析才截断）。
        if body.ends_with(b"\n") {
            return Ok(StatsLoadOutcome::Ready);
        }
        let tail_start = body
            .iter()
            .rposition(|&b| b == b'\n')
            .map(|i| i + 1)
            .unwrap_or(0);
        let tail = &body[tail_start..];
        if StatsEvent::parse_line(std::str::from_utf8(tail).unwrap_or("")).is_some() {
            // 末行可解析但缺结尾换行：补一个换行而非截断（内容完好）。
            let mut f = fs::OpenOptions::new().append(true).open(&self.path)?;
            f.write_all(b"\n")?;
            f.flush()?;
            return Ok(StatsLoadOutcome::Ready);
        }
        let truncated = (body.len() - tail_start) as u64;
        fs::write(&self.path, &body[..tail_start])?;
        Ok(StatsLoadOutcome::Repaired(truncated))
    }
}

/// 启动清理同目录孤儿 `*.tmp`（WP-A 原子写中途崩溃残留；尽力而为）。
fn cleanup_orphan_tmps(dir: &Path) {
    let Ok(entries) = fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.ends_with(".tmp") {
            let _ = fs::remove_file(e.path());
        }
    }
}

/// 读取端（归因/导出/复算共用）：同读 archive + current 两文件
///（v3 跨轮转闭合），skip-bad-lines 二道防线。返回按文件顺序
///（archive 先于 current）的事件序列。
pub fn load_events(dir: &Path) -> Vec<StatsEvent> {
    load_events_from(&dir.join(STATS_ARCHIVE_NAME), &dir.join(STATS_FILE_NAME))
}

/// load_events 的路径显式版（测试与导出复用）。
pub fn load_events_from(archive: &Path, current: &Path) -> Vec<StatsEvent> {
    let mut out = Vec::new();
    for path in [archive, current] {
        let Ok(body) = fs::read_to_string(path) else { continue };
        for line in body.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Some(ev) = StatsEvent::parse_line(line) {
                out.push(ev);
            }
            // 坏行跳过（skip-bad-lines）：不中断、不报错——一道防线
            //（启动截断）之外的双保险。
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "volttray-stats-test-{}-{}",
            tag,
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fast_tuning() -> FlushTuning {
        FlushTuning {
            per_device_debounce: Duration::from_secs(0),
            global_debounce: Duration::from_secs(0),
            flush_fallback: Duration::from_secs(0),
            fsync_interval: Duration::from_secs(0),
        }
    }

    #[test]
    fn event_line_roundtrip_all_types() {
        let cases = vec![
            StatsEvent::Read(ReadEvent {
                ts: 1_760_000_000,
                physical_key: "u123".into(),
                display_name: "PRO X 2".into(),
                percent: 82,
                charging: false,
                source: SOURCE_POLL.into(),
            }),
            StatsEvent::RunStart { ts: 1_760_000_001 },
            StatsEvent::RunStop { ts: 1_760_000_002 },
            StatsEvent::Boundary {
                ts: 1_760_000_003,
                devices: vec![
                    BoundaryDevice {
                        physical_key: "u1".into(),
                        percent: 50,
                        charging: false,
                    },
                    BoundaryDevice {
                        physical_key: "u2".into(),
                        percent: 60,
                        charging: true,
                    },
                ],
            },
            StatsEvent::SleepEnter {
                ts: 1_760_000_004,
                physical_key: "u123".into(),
            },
        ];
        for ev in cases {
            let line = ev.to_json_line();
            let back = StatsEvent::parse_line(&line).expect("roundtrip parses");
            assert_eq!(back, ev, "line: {line}");
        }
    }

    #[test]
    fn parse_line_validates_per_type_required_fields() {
        // 读取事件缺 source → 坏行
        assert!(StatsEvent::parse_line(
            r#"{"ts":1,"physical_key":"u","display_name":"n","percent":50,"charging":false}"#
        )
        .is_none());
        // source 非枚举 → 坏行
        assert!(StatsEvent::parse_line(
            r#"{"ts":1,"physical_key":"u","display_name":"n","percent":50,"charging":false,"source":"x"}"#
        )
        .is_none());
        // percent 超 100 → 坏行
        assert!(StatsEvent::parse_line(
            r#"{"ts":1,"physical_key":"u","display_name":"n","percent":101,"charging":false,"source":"poll"}"#
        )
        .is_none());
        // run_start 不得被读取事件字段集误杀（红队 P2-1）
        assert_eq!(
            StatsEvent::parse_line(r#"{"ts":1,"event":"run_start"}"#),
            Some(StatsEvent::RunStart { ts: 1 })
        );
        // sleep_enter 缺 physical_key → 坏行
        assert!(StatsEvent::parse_line(r#"{"ts":1,"event":"sleep_enter"}"#).is_none());
        // boundary 缺 devices → 坏行
        assert!(StatsEvent::parse_line(r#"{"ts":1,"event":"boundary"}"#).is_none());
        // 未知事件类型 → 保守跳过
        assert!(StatsEvent::parse_line(r#"{"ts":1,"event":"future_v9"}"#).is_none());
        // 缺 ts → 坏行
        assert!(StatsEvent::parse_line(r#"{"event":"run_start"}"#).is_none());
        // 非 JSON → 坏行
        assert!(StatsEvent::parse_line("not json at all").is_none());
        // 容忍额外字段（前向兼容）
        assert_eq!(
            StatsEvent::parse_line(r#"{"ts":1,"event":"run_stop","extra":true}"#),
            Some(StatsEvent::RunStop { ts: 1 })
        );
    }

    #[test]
    fn change_driven_filter_and_wake_first_read_exception() {
        let dir = tmpdir("filter");
        let mut st = StatsStore::with_tuning(&dir, fast_tuning());
        let t0 = Instant::now();
        // 首条（新键锚点）必记
        assert!(st.note_reading("u1", "A", 80, false, SOURCE_POLL, 100, t0));
        // 同值跳过
        assert!(!st.note_reading("u1", "A", 80, false, SOURCE_POLL, 110, t0));
        // percent 变化记录
        assert!(st.note_reading("u1", "A", 79, false, SOURCE_POLL, 120, t0));
        // charging 变化记录
        assert!(st.note_reading("u1", "A", 79, true, SOURCE_POLL, 130, t0));
        // 同值 wake-first-read 必记（休眠段终点锚点）
        assert!(st.note_reading("u1", "A", 79, true, SOURCE_WAKE_FIRST_READ, 140, t0));
        st.shutdown_flush(t0, 150).unwrap();
        let events = load_events(&dir);
        assert_eq!(events.len(), 4);
        assert!(matches!(events[3], StatsEvent::Read(ref e) if e.source == SOURCE_WAKE_FIRST_READ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn conflicted_keys_are_not_recorded() {
        let dir = tmpdir("conflict");
        let mut st = StatsStore::with_tuning(&dir, fast_tuning());
        let t0 = Instant::now();
        st.set_conflicted(HashSet::from(["bad".to_string()]));
        assert!(!st.note_reading("bad", "X", 10, false, SOURCE_POLL, 100, t0));
        st.note_sleep_enter("bad", 101, t0);
        assert!(st.note_reading("good", "Y", 90, false, SOURCE_POLL, 102, t0));
        st.shutdown_flush(t0, 103).unwrap();
        let events = load_events(&dir);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].physical_key(), Some("good"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn display_name_truncated_to_64_chars() {
        let dir = tmpdir("truncate");
        let mut st = StatsStore::with_tuning(&dir, fast_tuning());
        let t0 = Instant::now();
        let long = "鼠".repeat(100);
        st.note_reading("u1", &long, 50, false, SOURCE_POLL, 100, t0);
        st.shutdown_flush(t0, 101).unwrap();
        let events = load_events(&dir);
        let StatsEvent::Read(e) = &events[0] else {
            panic!("expected read event")
        };
        assert_eq!(e.display_name.chars().count(), MAX_NAME_CHARS);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotation_at_10mb_writes_boundary_head_and_keeps_two_files() {
        let dir = tmpdir("rotate");
        let mut st = StatsStore::with_tuning(&dir, fast_tuning());
        let t0 = Instant::now();
        // 先落一条已知读数（进 current）。
        st.note_reading("u1", "A", 55, false, SOURCE_POLL, 100, t0);
        st.shutdown_flush(t0, 101).unwrap();
        // 把 current 灌到贴近 10MB（构造大行快速灌满，避免慢循环）——
        // 灌到「再追加一行事件即触顶」的水位。
        let filler = "x".repeat((STATS_MAX_BYTES - 128) as usize);
        {
            let mut f = fs::OpenOptions::new().append(true).open(dir.join(STATS_FILE_NAME)).unwrap();
            // 单行非法事件（读取端会 skip）只用于占位灌满。
            f.write_all(filler.as_bytes()).unwrap();
            f.write_all(b"\n").unwrap();
        }
        // 再记一条触发轮转。
        st.note_reading("u1", "A", 54, false, SOURCE_POLL, 200, t0);
        let out = st.shutdown_flush(t0, 201).unwrap();
        assert!(out.rotated);
        // 文件数恒 2：archive（旧 current）+ 新 current。
        let archive = fs::metadata(dir.join(STATS_ARCHIVE_NAME)).unwrap();
        assert!(archive.len() > STATS_MAX_BYTES - 4096);
        let body = fs::read_to_string(dir.join(STATS_FILE_NAME)).unwrap();
        // 新文件头部是 boundary，携带轮转时刻最后读数。
        let first = body.lines().next().unwrap();
        let Some(StatsEvent::Boundary { ts, devices }) = StatsEvent::parse_line(first) else {
            panic!("first line of rotated file must be boundary, got: {first}")
        };
        assert_eq!(ts, 201);
        assert!(devices
            .iter()
            .any(|d| d.physical_key == "u1" && d.percent == 54 && !d.charging));
        // 单次 flush 从水位下直接跳窗触顶：轮转发生时旧归档同步被覆盖，
        // 不再补弹「即将轮转」（事后提示为伪陈述，质检 F2 口径）——护栏
        // toast 只由水位预提示在覆盖前发出（见下条用例）。
        assert!(!st.take_rotation_notice());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn rotation_guard_toast_precedes_archive_overwrite() {
        // v3 护栏时序（质检 F2）：toast 必须先于旧归档覆盖——水位预提示
        //（≥9MB 未触顶）置信号位时旧归档保持完好可导出；触顶轮转本身
        // 不置位；闩锁保证每轮 current 只提示一次。
        let dir = tmpdir("rotatewarn");
        let mut st = StatsStore::with_tuning(&dir, fast_tuning());
        let t0 = Instant::now();
        // 预置旧归档（模拟上一轮轮转产物），预提示期间必须保持完好。
        fs::write(dir.join(STATS_ARCHIVE_NAME), b"OLD-ARCHIVE-MARKER\n").unwrap();
        // 先落一条已知读数，再把 current 灌到水位下不远处。
        st.note_reading("u1", "A", 55, false, SOURCE_POLL, 100, t0);
        st.shutdown_flush(t0, 101).unwrap();
        let filler = "x".repeat((STATS_ROTATE_WARN_BYTES - 4096) as usize);
        {
            let mut f = fs::OpenOptions::new().append(true).open(dir.join(STATS_FILE_NAME)).unwrap();
            f.write_all(filler.as_bytes()).unwrap();
            f.write_all(b"\n").unwrap();
        }
        assert!(!st.take_rotation_notice(), "水位未到不提示");
        // 缓冲一批事件使预计落盘越过水位（9MB）但不触顶（10MB）。
        for i in 0..200 {
            st.note_reading("u1", "A", (i % 100) as u8, false, SOURCE_POLL, 200 + i, t0);
        }
        let out = st.shutdown_flush(t0, 300).unwrap();
        assert!(!out.rotated, "过水位未触顶不轮转");
        assert!(st.take_rotation_notice(), "越过水位须提示（覆盖旧归档前）");
        assert!(!st.take_rotation_notice(), "信号取走后复位");
        // 旧归档在提示发出时仍完好——用户可一键导出（v3 口径核心）。
        assert_eq!(
            fs::read(dir.join(STATS_ARCHIVE_NAME)).unwrap(),
            b"OLD-ARCHIVE-MARKER\n"
        );
        // 闩锁：本轮 current 内再次越水位落盘不重复提示。
        for i in 0..100 {
            st.note_reading("u1", "A", (i % 100) as u8, false, SOURCE_POLL, 400 + i, t0);
        }
        st.shutdown_flush(t0, 500).unwrap();
        assert!(!st.take_rotation_notice(), "闩锁期内不二次提示");
        // 灌到触顶：轮转覆盖旧归档，但不补弹提示。
        let cur_len = fs::metadata(dir.join(STATS_FILE_NAME)).unwrap().len();
        let filler2 = "x".repeat((STATS_MAX_BYTES - cur_len - 64) as usize);
        {
            let mut f = fs::OpenOptions::new().append(true).open(dir.join(STATS_FILE_NAME)).unwrap();
            f.write_all(filler2.as_bytes()).unwrap();
            f.write_all(b"\n").unwrap();
        }
        st.note_reading("u1", "A", 54, false, SOURCE_POLL, 600, t0);
        let out = st.shutdown_flush(t0, 601).unwrap();
        assert!(out.rotated, "触顶必须轮转");
        assert!(!st.take_rotation_notice(), "触顶轮转不补弹");
        assert_ne!(
            fs::read(dir.join(STATS_ARCHIVE_NAME)).unwrap(),
            b"OLD-ARCHIVE-MARKER\n",
            "轮转后旧归档已被覆盖"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dual_file_total_stays_within_20mb_under_sustained_load() {
        // AC-B7 规模口径：持续灌满轮转后双文件合计 ≤20MB（current ≤10MB
        // + archive 为上一轮换下的 current ≤10MB + 一次缓冲追加）。
        let dir = tmpdir("dualcap");
        let mut st = StatsStore::with_tuning(&dir, fast_tuning());
        let t0 = Instant::now();
        let line_len = {
            let ev = StatsEvent::Read(ReadEvent {
                ts: 1,
                physical_key: "u1".into(),
                display_name: "A".into(),
                percent: 50,
                charging: false,
                source: SOURCE_POLL.into(),
            });
            ev.to_json_line().len() as u64 + 1
        };
        // 注入约 25MB 事件（超过两轮轮转），每次百分之一变化保证记录。
        let total = (25 * 1024 * 1024) / line_len + 2;
        for i in 0..total {
            let pct = (i % 100) as u8;
            st.note_reading("u1", "A", pct, false, SOURCE_POLL, 100 + i as i64, t0);
            if i % 2000 == 0 {
                st.flush_if_due(t0, 100 + i as i64).unwrap();
            }
        }
        st.shutdown_flush(t0, 200 + total as i64).unwrap();
        let cur = fs::metadata(dir.join(STATS_FILE_NAME)).unwrap().len();
        let arc = fs::metadata(dir.join(STATS_ARCHIVE_NAME)).unwrap().len();
        assert!(cur <= STATS_MAX_BYTES, "current {cur} over 10MB");
        assert!(arc <= STATS_MAX_BYTES + line_len, "archive {arc} over cap");
        assert!(
            cur + arc <= 2 * STATS_MAX_BYTES + line_len,
            "dual total {} over 20MB",
            cur + arc
        );
        // 目录中统计文件数恒 ≤2。
        let stats_files = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                let n = e.file_name();
                let n = n.to_str().unwrap_or("");
                n.starts_with("stats")
            })
            .count();
        assert_eq!(stats_files, 2);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn half_line_repair_truncates_to_last_complete_line() {
        let dir = tmpdir("halftail");
        let path = dir.join(STATS_FILE_NAME);
        let good = StatsEvent::RunStart { ts: 1 }.to_json_line();
        let mut body = good.clone();
        body.push('\n');
        body.push_str("{\"ts\":2,\"physical_key\":\"u\",\"displa"); // 半截
        fs::write(&path, &body).unwrap();
        let (_st, outcome) = StatsStore::load(&dir, fast_tuning());
        let repaired_len = match outcome {
            StatsLoadOutcome::Repaired(n) => n,
            other => panic!("expected Repaired, got {other:?}"),
        };
        assert_eq!(repaired_len, "{\"ts\":2,\"physical_key\":\"u\",\"displa".len() as u64);
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(after, format!("{good}\n"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn repair_completes_missing_trailing_newline_instead_of_truncating() {
        let dir = tmpdir("nonewline");
        let path = dir.join(STATS_FILE_NAME);
        let good = StatsEvent::RunStop { ts: 7 }.to_json_line();
        fs::write(&path, &good).unwrap(); // 内容完好但无结尾换行
        let (_st, outcome) = StatsStore::load(&dir, fast_tuning());
        assert_eq!(outcome, StatsLoadOutcome::Ready);
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(after, format!("{good}\n"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_events_reads_archive_then_current_and_skips_bad_lines() {
        let dir = tmpdir("readboth");
        fs::write(
            dir.join(STATS_ARCHIVE_NAME),
            format!("{}\nbad line\n", StatsEvent::RunStart { ts: 1 }.to_json_line()),
        )
        .unwrap();
        fs::write(
            dir.join(STATS_FILE_NAME),
            format!("{}\n", StatsEvent::RunStop { ts: 2 }.to_json_line()),
        )
        .unwrap();
        let events = load_events(&dir);
        assert_eq!(
            events,
            vec![StatsEvent::RunStart { ts: 1 }, StatsEvent::RunStop { ts: 2 }]
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn flush_pacing_respects_global_debounce_and_fallback() {
        let dir = tmpdir("pacing");
        let tuning = FlushTuning {
            per_device_debounce: Duration::from_secs(30),
            global_debounce: Duration::from_secs(10),
            flush_fallback: Duration::from_secs(60),
            fsync_interval: Duration::from_secs(300),
        };
        let mut st = StatsStore::with_tuning(&dir, tuning);
        let t0 = Instant::now();
        st.note_reading("u1", "A", 80, false, SOURCE_POLL, 1, t0);
        // 立即 flush：首次落盘（last_flush None）允许。
        let out = st.flush_if_due(t0, 2).unwrap();
        assert!(out.wrote);
        // 10s 内再 flush：防抖拦截。
        st.note_reading("u1", "A", 79, false, SOURCE_POLL, 3, t0);
        let out = st.flush_if_due(t0 + Duration::from_secs(5), 4).unwrap();
        assert!(!out.wrote);
        // 过 10s 全局防抖：落盘。
        let out = st.flush_if_due(t0 + Duration::from_secs(11), 5).unwrap();
        assert!(out.wrote);
        // fsync 定时：首次落盘即附带 fsync（last_fsync None 视为到点，
        // 与 WP-A persist 同款语义）；此后有未 fsync 落盘且距上次 fsync
        // ≥300s 时才再 fsync。
        let dir2 = tmpdir("pacing-fsync");
        let mut st2 = StatsStore::with_tuning(&dir2, tuning);
        st2.note_reading("u1", "A", 80, false, SOURCE_POLL, 1, t0);
        let out = st2.flush_if_due(t0, 2).unwrap();
        assert!(out.wrote && out.fsynced, "first flush fsyncs");
        st2.note_reading("u1", "A", 79, false, SOURCE_POLL, 3, t0);
        let out = st2.flush_if_due(t0 + Duration::from_secs(11), 4).unwrap();
        assert!(out.wrote && !out.fsynced, "within fsync interval");
        let out = st2.flush_if_due(t0 + Duration::from_secs(301), 400).unwrap();
        assert!(out.fsynced, "fsync interval elapsed must fsync");
        let events = load_events(&dir);
        assert_eq!(events.len(), 2);
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&dir2);
    }
}
