//! volttray-app —— WP-B 耗电统计：分状态归因引擎与导出渲染。
//!
//! 口径来源（v3 设计文档 WP-B 判定表 + 确定性合并算法，唯一基线）：
//! - 读取时不预算，导出/展示时按时间轴分段；sleep_enter 为休眠段唯一
//!   起点（事实而非间隔推断）；段在 run_stop / run_start 边界强制断开；
//!   段边界即事件点，首尾相接不重叠不遗漏；展示与导出最小聚合窗口 1h。
//! - ≥1h 确定性合并算法（红队 P1-6 必修，AC-B2/B8 共同基准）：不足 1h
//!   的段并入时间相邻且类型相同的段（两侧均可并时并入时长较长一侧，
//!   等长取先）；无同型邻段保留并标 note=below_window；并入后整段仍
//!   不足 1h 同样标 below_window——一轮并入即止、不做二次级联（本轮
//!   已参与合并的段不再作为合并目标，消除并入次序依赖，复算脚本与
//!   应用内展示必然一致）；合并后 start_pct/end_pct 取整段端值，rate
//!   按整段 Δ%/Δt 重算。合并在段级进行、不涉日界。
//! - 判定表锚点：活跃=在线未充电相邻读取间 Δ%；休眠=sleep_enter 起至
//!   唤醒后首条读取止（Δ%=睡前最后一条−醒后首条）；充电=charging=true
//!   连续区间按整段 Δ%/Δt 均值；跨 run 缺口标未知 run_gap_unattributed
//!   （含崩溃路径「最后一条事件→run_start」）；sleep_enter 缺失标未知
//!   evidence_missing（不回退间隔推断）；Δt≤0 标未知 clock_anomaly、
//!   Δt 超 7 天标未知 gap_over_7d，不产生负速率；boundary 事件闭合的
//!   段端点标 note=boundary。进行中的尾段（最后锚点之后）不落段——
//!   无终点读数不可算 Δ，下一锚点出现时自然闭合（D-75）。
//! - 时钟回拨（Δt≤0）负时出口径（质检 F1 钉死，Rust 与 recheck.py
//!   逐值一致的唯一基准）：回拨段保留实际 ts 区间（end<start）作为
//!   异常证据，不钳位、不重排；duration_min 对负时长按数学向下取整
//!   （floor，即 `(secs+30).div_euclid(60)` ≡ Python `(dur+30)//60`）；
//!   coalesce 仅合并「两段均非倒置（end>=start）且时序邻接（后段起点
//!   不早于前段终点）」的同型段——倒置段不参与合并、独立保留，
//!   coalesce 绝不产出 end_ts < start_ts 的段。
//! - 匿名化（R5-I-04）：按 physical_key 哈希值（FNV-1a 64，16 字符
//!   hex）字典序排序后依次编号（D1=最小），不依赖首现顺序；仅
//!   device_key 列脱敏，display_name 保留真实值；开关覆盖 CSV / JSON
//!   导出 / HTML 报告三路与 volttray-data 数据块（R5-I-07）。
//!
//! 红线：纯主机侧记账回放，不新增任何设备查询。

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::Serialize;

use crate::stats::{StatsEvent, SOURCE_WAKE_FIRST_READ};

/// 展示/导出最小聚合窗口：1 小时（AC-B3）。
pub const MERGE_WINDOW_SECS: i64 = 3600;
/// Δt 超阈值：>7 天的段标未知（NTP/改时间防线，不产生负速率）。
pub const GAP_OVER_LIMIT_SECS: i64 = 7 * 86_400;
/// 图表①小时聚合桶（UTC 对齐；标签按本地偏移呈现，D-75）。
pub const CHART1_BUCKET_SECS: i64 = 3600;
/// 图表①默认展示窗口：近 90 天（可切换全量历史）。
pub const CHART1_DEFAULT_WINDOW_DAYS: i64 = 90;
/// US-B4 近期口径：近 7 天。
pub const RECENT_DAYS: i64 = 7;

/// CSV note 枚举（段来源标注；boundary = 段端点由 boundary 事件闭合）。
pub const NOTE_RUN_GAP: &str = "run_gap_unattributed";
pub const NOTE_EVIDENCE_MISSING: &str = "evidence_missing";
pub const NOTE_BOUNDARY: &str = "boundary";
pub const NOTE_BELOW_WINDOW: &str = "below_window";
pub const NOTE_CLOCK_ANOMALY: &str = "clock_anomaly";
pub const NOTE_GAP_OVER_7D: &str = "gap_over_7d";

/// 段类型（CSV segment_type 列取值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum SegmentType {
    #[serde(rename = "active")]
    Active,
    #[serde(rename = "sleep")]
    Sleep,
    #[serde(rename = "charging")]
    Charging,
    #[serde(rename = "unknown")]
    Unknown,
}

impl SegmentType {
    pub fn as_str(&self) -> &'static str {
        match self {
            SegmentType::Active => "active",
            SegmentType::Sleep => "sleep",
            SegmentType::Charging => "charging",
            SegmentType::Unknown => "unknown",
        }
    }
}

/// 聚合段（CSV 每行 = 一个聚合段；HTML 分段明细表同数据同列）。
#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    /// 设备身份键（physical_key；渲染时按匿名化映射为 D1/D2…）。
    pub device_key: String,
    pub display_name: String,
    pub seg_type: SegmentType,
    pub start_ts: i64,
    pub end_ts: i64,
    pub start_pct: u8,
    pub end_pct: u8,
    /// 段来源标注（note 枚举；below_window 由合并轮追加在最后）。
    pub notes: Vec<String>,
    /// 合并轮参与标记（单轮不级联：本轮已参与者不再作为合并目标）。
    pub(crate) merged: bool,
}

impl Segment {
    pub fn duration_secs(&self) -> i64 {
        self.end_ts - self.start_ts
    }

    /// duration_min 列：四舍五入到整数分钟（(secs+30)/60）；负时长
    /// （时钟回拨段）按数学向下取整 floor——div_euclid 与 recheck.py
    /// `(dur+30)//60` 同口径（质检 F1 钉死；非负值与截断除法等价，
    /// 如 -100s → -2 而非 -1）。
    pub fn duration_min(&self) -> i64 {
        (self.duration_secs() + 30).div_euclid(60)
    }

    /// delta_pct 列：end − start（带符号；耗电为负）。
    pub fn delta_pct(&self) -> i16 {
        self.end_pct as i16 - self.start_pct as i16
    }

    /// rate_pct_per_hour 列：整段 Δ%/Δt（%/h，2 位小数）；unknown 段
    /// 不可算 → None（CSV 留空、JSON null）。零时长段不产生速率。
    pub fn rate_pct_per_hour(&self) -> Option<f64> {
        if self.seg_type == SegmentType::Unknown || self.duration_secs() <= 0 {
            return None;
        }
        let hours = self.duration_secs() as f64 / 3600.0;
        Some(round2(self.delta_pct() as f64 / hours))
    }

    /// CSV note 列（`;` 连接）。
    pub fn note_text(&self) -> String {
        self.notes.join(";")
    }
}

pub(crate) fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

pub(crate) fn round4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

// ---------------------------------------------------------------------------
// 时间工具（无 chrono 依赖：epoch → 民用历 + 注入式本地偏移，跨平台可测）
// ---------------------------------------------------------------------------

/// Howard Hinnant civil_from_days：days since 1970-01-01 → (y, m, d)。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// RFC3339 本地偏移呈现：`2026-10-16T09:00:00+08:00`。
/// offset_minutes 由调用方注入（Windows 侧取系统时区；测试注入固定值）。
pub fn fmt_rfc3339(ts: i64, offset_minutes: i32) -> String {
    let local = ts + offset_minutes as i64 * 60;
    let days = local.div_euclid(86_400);
    let secs = local.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let sign = if offset_minutes < 0 { '-' } else { '+' };
    let off = offset_minutes.abs();
    format!(
        "{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}{sign}{:02}:{:02}",
        off / 60,
        off % 60
    )
}

/// 本地日期串 `YYYY-MM-DD`（图表②日界 = 本地 00:00，红队 P1-3）。
pub fn fmt_local_date(ts: i64, offset_minutes: i32) -> String {
    let local = ts + offset_minutes as i64 * 60;
    let (y, m, d) = civil_from_days(local.div_euclid(86_400));
    format!("{y:04}-{m:02}-{d:02}")
}

/// 本地日界（00:00 本地）对应的 epoch 秒：ts 所在本地日的起点。
fn local_day_start(ts: i64, offset_minutes: i32) -> i64 {
    let local = ts + offset_minutes as i64 * 60;
    let day_start_local = local.div_euclid(86_400) * 86_400;
    day_start_local - offset_minutes as i64 * 60
}

// ---------------------------------------------------------------------------
// 分段引擎
// ---------------------------------------------------------------------------

/// 锚点类别（段边界即事件点）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnchorKind {
    Read,
    SleepEnter,
    RunStart,
    RunStop,
    Boundary,
}

#[derive(Debug, Clone, Copy)]
struct Anchor {
    ts: i64,
    kind: AnchorKind,
    /// Read / Boundary 携带的读数。
    pct: Option<u8>,
    charging: Option<bool>,
    /// Read 的 source（wake-first-read 判定 evidence_missing 用）。
    wake_first_read: bool,
}

/// 单设备时间轴分段（元素级；合并轮在 `build_segments` 内随后进行）。
fn segment_device(key: &str, anchors: &[Anchor], display_name: &str) -> Vec<Segment> {
    let mut out: Vec<Segment> = Vec::new();
    // carried：最近已知读数（Read / Boundary 更新；段端点缺读数时沿用）。
    let mut carried: Option<(u8, bool)> = None;
    // sleeping：sleep_enter 已见且尚未被读取事件结束（跨 run 保持——
    // 休眠证据跨 run 缺口时缺口标未知，run 内部分仍按休眠归因）。
    let mut sleeping = false;
    let mut prev: Option<Anchor> = None;

    for &cur in anchors {
        if let Some(p) = prev {
            let dt = cur.ts - p.ts;
            let mut notes: Vec<String> = Vec::new();
            // 段类型判定（判定表）。
            let seg_type;
            let mut start_pct = carried.map(|(pct, _)| pct).unwrap_or(0);
            let mut end_pct = cur.pct.unwrap_or(start_pct);
            if cur.kind == AnchorKind::RunStart {
                // run 边界强制断开：prev 为 run_stop（正常退出）或其它
                //（崩溃/强杀无 run_stop：最后一条事件 → run_start 为离线
                // 段，语义等价）→ 未知（未细分）。
                seg_type = SegmentType::Unknown;
                notes.push(NOTE_RUN_GAP.to_string());
                end_pct = start_pct; // 缺口两端无读数证据，沿用最后已知
            } else if dt <= 0 {
                seg_type = SegmentType::Unknown;
                notes.push(NOTE_CLOCK_ANOMALY.to_string());
            } else if dt > GAP_OVER_LIMIT_SECS {
                seg_type = SegmentType::Unknown;
                notes.push(NOTE_GAP_OVER_7D.to_string());
            } else if carried.is_none() {
                // 段端点证据缺失（旧归档已被覆盖 / 半截行截断等极端路径）。
                seg_type = SegmentType::Unknown;
                notes.push(NOTE_EVIDENCE_MISSING.to_string());
            } else if sleeping {
                seg_type = SegmentType::Sleep;
            } else if cur.kind == AnchorKind::Read && cur.wake_first_read {
                // sleep_enter 缺失（事件丢失）时的唤醒首读：标未知，
                // 不回退间隔推断。
                seg_type = SegmentType::Unknown;
                notes.push(NOTE_EVIDENCE_MISSING.to_string());
            } else {
                let (_, was_charging) = carried.expect("carried checked above");
                seg_type = if was_charging {
                    SegmentType::Charging
                } else {
                    SegmentType::Active
                };
            }
            if cur.kind == AnchorKind::Boundary {
                notes.push(NOTE_BOUNDARY.to_string());
            }
            if seg_type == SegmentType::Unknown && end_pct < start_pct {
                // 未知段不产生速率，pct 两列仅作证据呈现，保持原样。
            }
            // RunStart 缺口段 pct：carried 缺失时两端 0（无证据）。
            if carried.is_none() {
                start_pct = 0;
                end_pct = 0;
            }
            out.push(Segment {
                device_key: key.to_string(),
                display_name: display_name.to_string(),
                seg_type,
                start_ts: p.ts,
                end_ts: cur.ts,
                start_pct,
                end_pct,
                notes,
                merged: false,
            });
        }
        // 状态更新（当前锚点）。
        match cur.kind {
            AnchorKind::Read => {
                carried = Some((
                    cur.pct.expect("read anchor carries pct"),
                    cur.charging.expect("read anchor carries charging"),
                ));
                sleeping = false; // 唤醒后首条读取结束休眠段
            }
            AnchorKind::Boundary => {
                carried = Some((
                    cur.pct.expect("boundary anchor carries pct"),
                    cur.charging.expect("boundary anchor carries charging"),
                ));
                // boundary 不是唤醒证据：sleeping 保持不变。
            }
            AnchorKind::SleepEnter => sleeping = true,
            AnchorKind::RunStart | AnchorKind::RunStop => {}
        }
        prev = Some(cur);
    }
    // 进行中的尾段（最后锚点之后）不落段：无终点读数不可算 Δ（D-75）。

    // 元素段合并（coalesce）：相邻同型段并为一段（boundary 闭合不强制
    // 分段，note 保留；run 边界的「强制断开」由 run_gap 未知段横在两侧
    // 之间天然保证——unknown 与前后 sleep/active 不同型，不会误并）。
    // 时序口径（质检 F1 钉死，recheck.py 逐行镜像同规则）：仅当两段均
    // 非倒置（end>=start）且时序邻接（后段起点不早于前段终点）才合并
    // ——时钟回拨产生的倒置段不参与合并、独立保留为异常证据；正常段
    // 合并后 end 必为后段终点（只进不退），coalesce 绝不产出
    // end_ts < start_ts 的段。
    let mut merged: Vec<Segment> = Vec::new();
    for seg in out {
        if let Some(last) = merged.last_mut() {
            let sequential = last.start_ts <= last.end_ts
                && seg.start_ts <= seg.end_ts
                && seg.start_ts >= last.end_ts;
            if last.seg_type == seg.seg_type && sequential {
                last.end_ts = seg.end_ts;
                last.end_pct = seg.end_pct;
                for n in seg.notes {
                    if !last.notes.contains(&n) {
                        last.notes.push(n);
                    }
                }
                continue;
            }
        }
        merged.push(seg);
    }
    merged
}

/// ≥1h 确定性合并（红队 P1-6 + 二轮复核补钉：单轮不级联）。
fn merge_below_window(segs: &mut Vec<Segment>) {
    let mut i = 0usize;
    while i < segs.len() {
        if segs[i].duration_secs() >= MERGE_WINDOW_SECS || segs[i].merged {
            i += 1;
            continue;
        }
        let t = segs[i].seg_type;
        let prev_ok = i > 0 && !segs[i - 1].merged && segs[i - 1].seg_type == t;
        let next_ok = i + 1 < segs.len() && !segs[i + 1].merged && segs[i + 1].seg_type == t;
        // 目标选择：两侧均可并时并入时长较长一侧，等长取先（prev）。
        let target_prev = match (prev_ok, next_ok) {
            (true, true) => segs[i - 1].duration_secs() >= segs[i + 1].duration_secs(),
            (true, false) => true,
            (false, true) => false,
            (false, false) => {
                // 无同型邻段：保留原样标 below_window。
                segs[i].notes.push(NOTE_BELOW_WINDOW.to_string());
                i += 1;
                continue;
            }
        };
        let (winner, loser) = if target_prev { (i - 1, i) } else { (i + 1, i) };
        let loser_seg = segs.remove(loser);
        // loser 恒为当前段（索引 i）；target_next 时 winner=i+1 在 remove 后
        // 前移一位到 i——需同步校正，否则越界/错指。
        let winner = if winner > loser { winner - 1 } else { winner };
        let w = &mut segs[winner];
        if loser_seg.start_ts < w.start_ts {
            w.start_ts = loser_seg.start_ts;
            w.start_pct = loser_seg.start_pct;
        }
        if loser_seg.end_ts > w.end_ts {
            w.end_ts = loser_seg.end_ts;
            w.end_pct = loser_seg.end_pct;
        }
        for n in loser_seg.notes {
            if !w.notes.contains(&n) {
                w.notes.push(n);
            }
        }
        w.merged = true; // 单轮不级联：已参与者不再作为合并目标
                         // 并入后整段仍不足 1h：标 below_window（不二次级联）。
        if w.duration_secs() < MERGE_WINDOW_SECS
            && !w.notes.contains(&NOTE_BELOW_WINDOW.to_string())
        {
            w.notes.push(NOTE_BELOW_WINDOW.to_string());
        }
        // winner 已参与，扫指针越过它；prev 胜出时 i 已指向原 i+1。
        if !target_prev {
            i += 1;
        }
    }
    for s in segs.iter_mut() {
        s.merged = false; // 复位内部标记（不影响输出）
    }
}

/// 分段引擎入口：同读 current+archive 的事件序列（skip-bad-lines 已在
/// 读取端完成）→ 每设备时间轴分段 → coalesce → ≥1h 确定性合并 →
/// 按设备键 + start_ts 排序输出。
pub fn build_segments(events: &[StatsEvent]) -> Vec<Segment> {
    // 每设备锚点序列（保持事件原序——Δt≤0 检测依赖原始时序）。
    let mut per_device: BTreeMap<String, Vec<Anchor>> = BTreeMap::new();
    let mut names: BTreeMap<String, String> = BTreeMap::new();
    // 全局 run 标记（适用所有设备；含「设备尚无任何事件但 run 存在」的
    // 情形——无设备事件的 run 标记不产段，由首个设备锚点前的 prev=None
    // 自然跳过）。
    let mut run_marks: Vec<(i64, AnchorKind)> = Vec::new();
    for ev in events {
        match ev {
            StatsEvent::Read(e) => {
                per_device
                    .entry(e.physical_key.clone())
                    .or_default()
                    .push(Anchor {
                        ts: e.ts,
                        kind: AnchorKind::Read,
                        pct: Some(e.percent),
                        charging: Some(e.charging),
                        wake_first_read: e.source == SOURCE_WAKE_FIRST_READ,
                    });
                names.insert(e.physical_key.clone(), e.display_name.clone());
            }
            StatsEvent::SleepEnter { ts, physical_key } => {
                per_device
                    .entry(physical_key.clone())
                    .or_default()
                    .push(Anchor {
                        ts: *ts,
                        kind: AnchorKind::SleepEnter,
                        pct: None,
                        charging: None,
                        wake_first_read: false,
                    });
            }
            StatsEvent::Boundary { ts, devices } => {
                for d in devices {
                    per_device
                        .entry(d.physical_key.clone())
                        .or_default()
                        .push(Anchor {
                            ts: *ts,
                            kind: AnchorKind::Boundary,
                            pct: Some(d.percent),
                            charging: Some(d.charging),
                            wake_first_read: false,
                        });
                }
            }
            StatsEvent::RunStart { ts } => run_marks.push((*ts, AnchorKind::RunStart)),
            StatsEvent::RunStop { ts } => run_marks.push((*ts, AnchorKind::RunStop)),
        }
    }
    // run 标记按 ts 稳定归并进各设备锚点流（同 ts 排在设备事件之后——
    // run_stop 前的读数先闭合段）。设备锚点保持事件流原序、不按 ts 重排：
    // Δt≤0 检测（clock_anomaly）依赖相邻锚点的原始时序，排序会让倒流
    // 静默消失（二轮复核补钉）。
    let mut out: Vec<Segment> = Vec::new();
    for (key, mut anchors) in per_device {
        let mut combined: Vec<Anchor> = std::mem::take(&mut anchors);
        for (ts, kind) in &run_marks {
            let pos = combined
                .iter()
                .position(|a| a.ts > *ts)
                .unwrap_or(combined.len());
            combined.insert(
                pos,
                Anchor {
                    ts: *ts,
                    kind: *kind,
                    pct: None,
                    charging: None,
                    wake_first_read: false,
                },
            );
        }
        let name = names.get(&key).cloned().unwrap_or_default();
        let mut segs = segment_device(&key, &combined, &name);
        merge_below_window(&mut segs);
        out.extend(segs);
    }
    out.sort_by(|a, b| {
        a.device_key
            .cmp(&b.device_key)
            .then(a.start_ts.cmp(&b.start_ts))
    });
    out
}

// ---------------------------------------------------------------------------
// 匿名化（R5-I-04：哈希值字典序排序编号，D1=最小；覆盖 CSV/JSON/HTML）
// ---------------------------------------------------------------------------

/// FNV-1a 64 位哈希 → 16 字符小写 hex（确定性编号算法的排序键）。
pub fn fnv1a64_hex(s: &str) -> String {
    let mut h: u64 = 14_695_981_039_346_656_037;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(1_099_511_628_211);
    }
    format!("{h:016x}")
}

/// 匿名化映射：按哈希值字典序排序依次编号（D1=字典序最小）；哈希碰撞
/// 时按原键字典序决胜（保持确定性）。同一设备键集跨多次导出编号恒定
///（首现顺序漂移不影响；键集变化时编号可能平移——算法本身由 v3 钉死，
/// 该性质如实记录于 D-75）。
pub fn anonymize_map(keys: &BTreeSet<String>) -> HashMap<String, String> {
    let mut hashed: Vec<(String, &String)> = keys.iter().map(|k| (fnv1a64_hex(k), k)).collect();
    hashed.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    hashed
        .into_iter()
        .enumerate()
        .map(|(i, (_, k))| (k.clone(), format!("D{}", i + 1)))
        .collect()
}

/// 事件集中出现的全部设备键（匿名化键集 = 导出数据全集，CSV/JSON/HTML
/// 三路共用同一映射）。
pub fn event_keys(events: &[StatsEvent]) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    for ev in events {
        match ev {
            StatsEvent::Read(e) => {
                keys.insert(e.physical_key.clone());
            }
            StatsEvent::SleepEnter { physical_key, .. } => {
                keys.insert(physical_key.clone());
            }
            StatsEvent::Boundary { devices, .. } => {
                for d in devices {
                    keys.insert(d.physical_key.clone());
                }
            }
            _ => {}
        }
    }
    keys
}

// ---------------------------------------------------------------------------
// 图表底层数据（volttray-data 数据块内容；AC-B8 复算对账唯一取数源）
// ---------------------------------------------------------------------------

/// 图表①单设备序列：小时聚合点 + 未知/充电段背景区间。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Chart1Series {
    pub device_key: String,
    pub display_name: String,
    /// [bucket_start_ts, pct|null]：桶内取值 = 覆盖桶的最后一段的段内末值
    ///（长段规则 R5-P-01）；未知段与无数据桶为 null（断线）。
    pub points: Vec<(i64, Option<u8>)>,
    /// 未知段区间（灰罩）。
    pub unknown_spans: Vec<(i64, i64)>,
    /// 充电段区间（底色标记）。
    pub charging_spans: Vec<(i64, i64)>,
}

/// 图表①：按小时聚合取末值降采样，与分段数据同源。
fn build_chart1(segments: &[Segment], devices: &[(String, String)]) -> Vec<Chart1Series> {
    let (min_ts, max_ts) = match (
        segments.iter().map(|s| s.start_ts).min(),
        segments.iter().map(|s| s.end_ts).max(),
    ) {
        (Some(a), Some(b)) => (a, b),
        _ => return Vec::new(),
    };
    let first_bucket = min_ts.div_euclid(CHART1_BUCKET_SECS);
    let last_bucket = max_ts.div_euclid(CHART1_BUCKET_SECS);
    let mut out = Vec::new();
    for (key, name) in devices {
        let segs: Vec<&Segment> = segments.iter().filter(|s| &s.device_key == key).collect();
        let mut points = Vec::new();
        for b in first_bucket..=last_bucket {
            let b0 = b * CHART1_BUCKET_SECS;
            let b1 = b0 + CHART1_BUCKET_SECS;
            // 覆盖桶的最后一段（段内末值规则：值 = 该段 end_pct）。
            let last = segs
                .iter()
                .filter(|s| s.start_ts < b1 && s.end_ts > b0)
                .next_back();
            let v = match last {
                Some(s) if s.seg_type != SegmentType::Unknown => Some(s.end_pct),
                _ => None,
            };
            points.push((b0, v));
        }
        let span = |t: SegmentType| -> Vec<(i64, i64)> {
            segs.iter()
                .filter(|s| s.seg_type == t)
                .map(|s| (s.start_ts, s.end_ts))
                .collect()
        };
        out.push(Chart1Series {
            device_key: key.clone(),
            display_name: name.clone(),
            points,
            unknown_spans: span(SegmentType::Unknown),
            charging_spans: span(SegmentType::Charging),
        });
    }
    out
}

/// 图表②单日单设备 drain 值（活跃/休眠/未知三层，%/日，4 位小数）。
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DayDrain {
    pub active: f64,
    pub sleep: f64,
    pub unknown: f64,
}

/// 图表②：每日耗电对比。日界 = 本地 00:00（按事件 ts 本地时区）；
/// 跨日段 Δ% 按线性摊入并标近似（approx=true）；未知段独立灰色层，
/// 不并入活跃/休眠；充电段不计耗电（增益，不入本图）。drain =
/// max(0, start−end)（电量回升不产生负耗电）。
fn build_chart2(
    segments: &[Segment],
    offset_minutes: i32,
) -> (BTreeMap<String, BTreeMap<String, DayDrain>>, bool) {
    let mut days: BTreeMap<String, BTreeMap<String, DayDrain>> = BTreeMap::new();
    let mut approx = false;
    for seg in segments {
        let dur = seg.duration_secs();
        if dur <= 0 {
            continue;
        }
        let drain = (seg.start_pct as f64 - seg.end_pct as f64).max(0.0);
        let mut t = seg.start_ts;
        while t < seg.end_ts {
            let next_boundary = local_day_start(t, offset_minutes) + 86_400;
            let piece_end = next_boundary.min(seg.end_ts);
            if piece_end < seg.end_ts {
                approx = true; // 跨日段线性摊入，标近似
            }
            let frac = (piece_end - t) as f64 / dur as f64;
            let day = fmt_local_date(t, offset_minutes);
            let cell = days
                .entry(day)
                .or_default()
                .entry(seg.device_key.clone())
                .or_default();
            let v = drain * frac;
            match seg.seg_type {
                SegmentType::Active => cell.active += v,
                SegmentType::Sleep => cell.sleep += v,
                SegmentType::Unknown => cell.unknown += v,
                SegmentType::Charging => {} // 增益不入耗电对比
            }
            t = piece_end;
        }
    }
    // 统一 round4（复算脚本逐值一致的 rounding 口径）。
    for per_dev in days.values_mut() {
        for cell in per_dev.values_mut() {
            cell.active = round4(cell.active);
            cell.sleep = round4(cell.sleep);
            cell.unknown = round4(cell.unknown);
        }
    }
    (days, approx)
}

/// 图表③：时长占比（秒）。
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct TypeDurations {
    pub active: i64,
    pub sleep: i64,
    pub charging: i64,
    pub unknown: i64,
}

fn build_chart3(segments: &[Segment]) -> (BTreeMap<String, TypeDurations>, TypeDurations) {
    let mut per_dev: BTreeMap<String, TypeDurations> = BTreeMap::new();
    let mut total = TypeDurations::default();
    for s in segments {
        let d = s.duration_secs().max(0);
        let cell = per_dev.entry(s.device_key.clone()).or_default();
        let (a, b) = match s.seg_type {
            SegmentType::Active => (&mut cell.active, &mut total.active),
            SegmentType::Sleep => (&mut cell.sleep, &mut total.sleep),
            SegmentType::Charging => (&mut cell.charging, &mut total.charging),
            SegmentType::Unknown => (&mut cell.unknown, &mut total.unknown),
        };
        *a += d;
        *b += d;
    }
    (per_dev, total)
}

/// US-B4 近期耗速率：近 7 天活跃段耗速率（时间加权 = 窗口内活跃段
/// drain 总和 / 活跃小时总和；段归属按 start_ts 在窗口内计）。不足 7
/// 天按实际天数并在 meta 标注。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecentRate {
    /// %/h（2 位小数；窗口内无活跃段为 None）。
    pub rate_pct_per_hour: Option<f64>,
    /// 数据实际覆盖天数（不足 7 天标注用）。
    pub days_covered: i64,
}

fn build_recent_rates(
    segments: &[Segment],
    now_ref: i64,
    offset_minutes: i32,
) -> BTreeMap<String, RecentRate> {
    let window_start = now_ref - RECENT_DAYS * 86_400;
    // 数据覆盖天数 = 段覆盖的去重本地日数（与图表②同口径）。
    let mut all_days: BTreeSet<String> = BTreeSet::new();
    for s in segments {
        let mut t = s.start_ts;
        while t < s.end_ts.max(s.start_ts + 1) && t <= s.end_ts {
            all_days.insert(fmt_local_date(t, offset_minutes));
            let next = local_day_start(t, offset_minutes) + 86_400;
            if next <= t {
                break;
            }
            t = next;
        }
    }
    let days_covered = (all_days.len() as i64)
        .min(RECENT_DAYS)
        .max(if all_days.is_empty() { 0 } else { 1 });
    // 显式累计（Σdrain, Σhours）后一次求商（时间加权均值）。
    let mut acc: BTreeMap<String, (f64, f64)> = BTreeMap::new();
    for s in segments {
        if s.seg_type != SegmentType::Active || s.start_ts < window_start {
            continue;
        }
        let e = acc.entry(s.device_key.clone()).or_default();
        e.0 += (s.start_pct as f64 - s.end_pct as f64).max(0.0);
        e.1 += s.duration_secs() as f64 / 3600.0;
    }
    let mut out: BTreeMap<String, RecentRate> = BTreeMap::new();
    let mut devices: BTreeSet<String> = segments.iter().map(|s| s.device_key.clone()).collect();
    for k in acc.keys() {
        devices.insert(k.clone());
    }
    for k in devices {
        let rate = acc.get(&k).and_then(|(drain, hours)| {
            if *hours > 0.0 {
                Some(round2(drain / hours))
            } else {
                None
            }
        });
        out.insert(
            k,
            RecentRate {
                rate_pct_per_hour: rate,
                days_covered,
            },
        );
    }
    out
}

// ---------------------------------------------------------------------------
// CSV 导出（蓝队场景 11 列 schema，AC-B2 对账基准；UTF-8 with BOM）
// ---------------------------------------------------------------------------

/// CSV 表头（11 列；空态 0 段时仅输出本行，R5-P-03）。
pub const CSV_HEADER: &str = "device_key,display_name,segment_type,start_ts,end_ts,duration_min,start_pct,end_pct,delta_pct,rate_pct_per_hour,note";

/// CSV 字段转义（含 `,` `"` 换行时加引号、内引号双写）。
///
/// E12（报告口径收窄）：公式注入兜底仅针对 `+` / `-` 两前缀加 `'`
/// 引前缀——`=` 与 `@` 在设备名 sanitize 白名单阶段已被替换为 `?`
/// （见 device::sanitize::is_allowed），不可能活到导出侧；`+`/`-`
/// 是白名单放行字符（电量百分比、型号名常见），需在导出侧兜底。
/// 本守卫只覆盖用户可见/可被引用的 CSV 导出（render_csv），不影响
/// 内部诊断 JSON。
fn csv_field(s: &str) -> String {
    // 先加公式注入引前缀，再走引号包裹——`'+-` 开头即被表格软件当纯文本。
    let guarded = if s.starts_with('+') || s.starts_with('-') {
        format!("'{s}")
    } else {
        s.to_string()
    };
    if guarded.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", guarded.replace('"', "\"\""))
    } else {
        guarded
    }
}

/// 段的展示键（匿名化开启时映射 D1/D2…，否则原 physical_key）。
fn display_key(key: &str, anon: Option<&HashMap<String, String>>) -> String {
    anon.and_then(|m| m.get(key).cloned())
        .unwrap_or_else(|| key.to_string())
}

/// 单行 CSV（unknown 段 rate 留空）。
fn csv_row(seg: &Segment, key: &str, offset_minutes: i32) -> String {
    let rate = seg
        .rate_pct_per_hour()
        .map(|r| format!("{r:.2}"))
        .unwrap_or_default();
    [
        csv_field(key),
        csv_field(&seg.display_name),
        seg.seg_type.as_str().to_string(),
        fmt_rfc3339(seg.start_ts, offset_minutes),
        fmt_rfc3339(seg.end_ts, offset_minutes),
        seg.duration_min().to_string(),
        seg.start_pct.to_string(),
        seg.end_pct.to_string(),
        seg.delta_pct().to_string(),
        rate,
        csv_field(&seg.note_text()),
    ]
    .join(",")
}

/// 渲染 CSV：BOM + 表头 + 每段一行（行序 = 展示键字典序 + start_ts；
/// 匿名化后 D1/D2… 顺序即哈希字典序）。
pub fn render_csv(
    segments: &[Segment],
    anon: Option<&HashMap<String, String>>,
    offset_minutes: i32,
) -> String {
    let mut rows: Vec<(String, &Segment)> = segments
        .iter()
        .map(|s| (display_key(&s.device_key, anon), s))
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.start_ts.cmp(&b.1.start_ts)));
    let mut out = String::from("\u{feff}");
    out.push_str(CSV_HEADER);
    out.push('\n');
    for (key, seg) in rows {
        out.push_str(&csv_row(seg, &key, offset_minutes));
        out.push('\n');
    }
    out
}

/// 导出默认文件名（R5-P-02：秒级时间戳，与 v0.3.15 诊断导出精度对齐，
/// 消除同分钟撞名）：`volttray-stats-YYYYMMDD-HHMMSS.csv`。HTML/JSON
/// 主干跟随用户在 Save-As 中最终确认的 CSV 文件名（不在此处拼）。
pub fn stats_export_filename_local(p: crate::diag_export::LocalTimeParts) -> String {
    format!(
        "volttray-stats-{:04}{:02}{:02}-{:02}{:02}{:02}.csv",
        p.year, p.month, p.day, p.hour, p.minute, p.second
    )
}

// ---------------------------------------------------------------------------
// JSON 导出（原始事件序列；匿名化开关覆盖 physical_key 字段，R5-I-07）
// ---------------------------------------------------------------------------

/// 渲染 JSON 导出：`{version, exported_at, events:[...]}`——events 为
/// current+archive 原始事件序列（与 jsonl 行同构）；空态输出空结构
///（events: []，R5-P-03）。匿名化开启时 physical_key 字段（读取事件、
/// sleep_enter、boundary.devices[]）按同一 D1/D2 映射脱敏，
/// display_name 保留真实值。
pub fn render_json_events(
    events: &[StatsEvent],
    anon: Option<&HashMap<String, String>>,
    exported_at: i64,
) -> String {
    let mut arr = Vec::with_capacity(events.len());
    for ev in events {
        let v = match ev {
            StatsEvent::Read(e) => serde_json::json!({
                "ts": e.ts,
                "physical_key": display_key(&e.physical_key, anon),
                "display_name": e.display_name,
                "percent": e.percent,
                "charging": e.charging,
                "source": e.source,
            }),
            StatsEvent::RunStart { ts } => serde_json::json!({"ts": ts, "event": "run_start"}),
            StatsEvent::RunStop { ts } => serde_json::json!({"ts": ts, "event": "run_stop"}),
            StatsEvent::Boundary { ts, devices } => {
                let devs: Vec<serde_json::Value> = devices
                    .iter()
                    .map(|d| {
                        serde_json::json!({
                            "physical_key": display_key(&d.physical_key, anon),
                            "percent": d.percent,
                            "charging": d.charging,
                        })
                    })
                    .collect();
                serde_json::json!({"ts": ts, "event": "boundary", "devices": devs})
            }
            StatsEvent::SleepEnter { ts, physical_key } => serde_json::json!({
                "ts": ts,
                "physical_key": display_key(physical_key, anon),
                "event": "sleep_enter",
            }),
        };
        arr.push(v);
    }
    serde_json::json!({
        "version": 1,
        "exported_at": exported_at,
        "events": arr,
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// HTML 可视化报告（自包含单文件，内联 SVG/JS，零外部依赖）
// ---------------------------------------------------------------------------

/// volttray-data 机器可读数据块（AC-B8 复算对账唯一取数源）：分段明细
/// （与 CSV 同数据同列）+ 图表①②③底层数据 + US-B4 近 7 天耗速率。
/// 匿名化开启时仅含匿名化后键，不含 physical_key 明文（R5-I-04 并入）。
pub fn build_report_data(
    events: &[StatsEvent],
    segments: &[Segment],
    anon: Option<&HashMap<String, String>>,
    offset_minutes: i32,
    generated_ts: i64,
) -> serde_json::Value {
    let anonymized = anon.is_some();
    // 设备清单（展示键 + 显示名；序 = 展示键字典序）。
    let mut name_of: BTreeMap<String, String> = BTreeMap::new();
    for ev in events {
        if let StatsEvent::Read(e) = ev {
            name_of.insert(display_key(&e.physical_key, anon), e.display_name.clone());
        }
    }
    let devices: Vec<(String, String)> = name_of.into_iter().collect();
    // 段键改展示键（排序 = 展示键 + start_ts，与 CSV 同序）。
    let mut segs: Vec<Segment> = segments
        .iter()
        .map(|s| {
            let mut c = s.clone();
            c.device_key = display_key(&s.device_key, anon);
            c
        })
        .collect();
    segs.sort_by(|a, b| {
        a.device_key
            .cmp(&b.device_key)
            .then(a.start_ts.cmp(&b.start_ts))
    });
    let seg_rows: Vec<serde_json::Value> = segs
        .iter()
        .map(|s| {
            serde_json::json!({
                "device_key": s.device_key,
                "display_name": s.display_name,
                "segment_type": s.seg_type.as_str(),
                "start_ts": s.start_ts,
                "end_ts": s.end_ts,
                "duration_min": s.duration_min(),
                "start_pct": s.start_pct,
                "end_pct": s.end_pct,
                "delta_pct": s.delta_pct(),
                "rate_pct_per_hour": s.rate_pct_per_hour(),
                "note": s.note_text(),
            })
        })
        .collect();
    let chart1 = build_chart1(&segs, &devices);
    let (chart2_days, chart2_approx) = build_chart2(&segs, offset_minutes);
    let (chart3_per_dev, chart3_total) = build_chart3(&segs);
    let now_ref = events.iter().map(|e| e.ts()).max().unwrap_or(generated_ts);
    let recent = build_recent_rates(&segs, now_ref, offset_minutes);
    let days_covered = recent.values().map(|r| r.days_covered).max().unwrap_or(0);
    let sparse_note = if segments.is_empty() {
        None
    } else if days_covered < RECENT_DAYS {
        Some(format!(
            "数据不足 {RECENT_DAYS} 天，按实际 {days_covered} 天统计"
        ))
    } else {
        None
    };
    let tz = {
        let sign = if offset_minutes < 0 { '-' } else { '+' };
        let o = offset_minutes.abs();
        format!("{sign}{:02}:{:02}", o / 60, o % 60)
    };
    serde_json::json!({
        "meta": {
            "version": 1,
            "generated_at": generated_ts,
            "generated_at_local": fmt_rfc3339(generated_ts, offset_minutes),
            "tz_offset": tz,
            "anonymized": anonymized,
            "days_covered": days_covered,
            "sparse_note": sparse_note,
            "chart2_approx": chart2_approx,
            "chart1_bucket_secs": CHART1_BUCKET_SECS,
            "chart1_default_window_days": CHART1_DEFAULT_WINDOW_DAYS,
            "recent_days": RECENT_DAYS,
        },
        "devices": devices,
        "segments": seg_rows,
        "chart1": chart1,
        "chart2": chart2_days,
        "chart3": { "per_device": chart3_per_dev, "total": chart3_total },
        "recent7d_active_rate": recent,
    })
}

/// HTML 转义（显示名等用户可控文本进 HTML 前必经此）。
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 渲染自包含 HTML 报告。数据经 `<script id="volttray-data"
/// type="application/json">` 注入（JSON 内 `<` 转 \u003c，防 `</script>`
/// 截断）；四类图表 + 空态「暂无数据」占位均由内联 JS 从该数据块渲染
///（单一取数源，与 CSV 天然同数据）。模板零外链：无 http(s)、无
/// link/img/script src、无 CSS url()/@import——断网双击可开（AC-B7）。
pub fn render_html(report_data: &serde_json::Value) -> String {
    let data_json = report_data.to_string().replace('<', "\\u003c");
    let meta = &report_data["meta"];
    let title_bits = format!(
        "生成于 {} · 数据覆盖 {} 天 · 匿名化：{}",
        html_escape(meta["generated_at_local"].as_str().unwrap_or("")),
        meta["days_covered"].as_i64().unwrap_or(0),
        if meta["anonymized"].as_bool().unwrap_or(false) {
            "开"
        } else {
            "关"
        }
    );
    let sparse = meta["sparse_note"]
        .as_str()
        .map(html_escape)
        .unwrap_or_default();
    HTML_TEMPLATE
        .replace("__TITLE_BITS__", &title_bits)
        .replace("__SPARSE_NOTE__", &sparse)
        .replace("__VOLTTRAY_DATA__", &data_json)
}

const HTML_TEMPLATE: &str = r##"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>LogiVoltTray 耗电统计报告</title>
<style>
:root{color-scheme:light dark}
body{font-family:"Segoe UI","Microsoft YaHei",system-ui,sans-serif;margin:0;padding:24px;max-width:1080px;margin:0 auto;line-height:1.5}
h1{font-size:20px;margin:0 0 4px}h2{font-size:16px;margin:24px 0 8px}
.meta{color:#666;font-size:13px}.sparse{color:#b26a00;font-size:13px}
.cards{display:flex;flex-wrap:wrap;gap:12px;margin:12px 0}
.card{border:1px solid #ccc5;border-radius:8px;padding:10px 14px;min-width:150px}
.card .k{font-size:12px;color:#777}.card .v{font-size:18px;font-weight:600}
.controls button{margin-right:8px;padding:4px 12px;border:1px solid #9995;border-radius:6px;background:transparent;color:inherit;cursor:pointer}
.controls button.on{border-color:#2b6cb0;color:#2b6cb0;font-weight:600}
.scroll{overflow-x:auto;border:1px solid #ccc5;border-radius:8px;padding:8px}
svg{display:block}
.hint{font-size:12px;color:#777;margin:6px 0}
.empty{border:1px dashed #bbb8;border-radius:8px;padding:32px;text-align:center;color:#888}
table{border-collapse:collapse;width:100%;font-size:12.5px}
th,td{border:1px solid #ccc5;padding:3px 8px;text-align:left;white-space:nowrap}
th{position:sticky;top:0;background:Canvas}
.tblwrap{max-height:480px;overflow:auto;border-radius:8px}
.legend span{display:inline-block;margin-right:14px;font-size:12px}
.legend i{display:inline-block;width:10px;height:10px;border-radius:2px;margin-right:4px}
</style>
</head>
<body>
<header>
<h1>LogiVoltTray 耗电统计报告</h1>
<p class="meta">__TITLE_BITS__</p>
<p class="sparse">__SPARSE_NOTE__</p>
</header>
<section class="cards" id="summary"></section>
<h2>① 电量曲线（按小时聚合取末值）</h2>
<div class="controls" id="c1ctl">
<button data-win="90" class="on">近 90 天</button><button data-win="30">近 30 天</button><button data-win="0">全部历史</button>
</div>
<div class="scroll"><svg id="c1" width="960" height="280" role="img" aria-label="电量曲线"></svg></div>
<p class="hint">跨未知段断线并以灰罩标示（不直线相连）；充电段以底色标记；窗口可切换，默认近 90 天。</p>
<h2>② 每日耗电对比（活跃 / 休眠 / 未知）</h2>
<div class="scroll"><svg id="c2" width="960" height="260" role="img" aria-label="每日耗电对比"></svg></div>
<p class="hint">日界按本地 00:00 切段，跨日段 Δ% 按线性摊入（近似）；未知段为灰色层，不并入活跃/休眠；不足 7 天按实际天数。</p>
<h2>③ 时长占比</h2>
<div class="scroll"><svg id="c3" width="960" height="180" role="img" aria-label="时长占比"></svg></div>
<div class="legend" id="c3legend"></div>
<h2>④ 分段明细（与 CSV 同数据同列）</h2>
<div class="tblwrap"><table id="seg"><thead><tr>
<th>device_key</th><th>display_name</th><th>segment_type</th><th>start_ts</th><th>end_ts</th><th>duration_min</th><th>start_pct</th><th>end_pct</th><th>delta_pct</th><th>rate_pct_per_hour</th><th>note</th>
</tr></thead><tbody></tbody></table></div>
<script id="volttray-data" type="application/json">__VOLTTRAY_DATA__</script>
<script>
"use strict";
var D=JSON.parse(document.getElementById("volttray-data").textContent);
var PALETTE=["#2b6cb0","#c53030","#2f855a","#b7791f","#6b46c1","#c05621","#2c7a7b","#97266d"];
var TYPE_COLOR={"active":"#2b6cb0","sleep":"#6b46c1","charging":"#2f855a","unknown":"#a0aec0"};
function esc(s){return String(s).replace(/&/g,"&amp;").replace(/</g,"&lt;").replace(/>/g,"&gt;");}
function el(tag,attrs,parent){var e=document.createElementNS("http://www.w3.org/2000/svg",tag);for(var k in attrs)e.setAttribute(k,attrs[k]);if(parent)parent.appendChild(e);return e;}
function fmtTs(ts){var m=D.meta.tz_offset;var sign=m[0]==="-"?-1:1;var p=m.slice(1).split(":");var off=(+p[0]*60+(+p[1]))*sign*60;var d=new Date((ts+off)*1000);function z(n){return (n<10?"0":"")+n;}return d.getUTCFullYear()+"-"+z(d.getUTCMonth()+1)+"-"+z(d.getUTCDate())+" "+z(d.getUTCHours())+":"+z(d.getUTCMinutes());}
function emptyMark(id){var n=document.getElementById(id);if(n)n.outerHTML='<div class="empty">暂无数据</div>';}
var segs=D.segments||[];
// 摘要卡
(function(){
  var s=document.getElementById("summary");var html="";
  html+='<div class="card"><div class="k">设备数</div><div class="v">'+(D.devices||[]).length+"</div></div>";
  html+='<div class="card"><div class="k">聚合段数</div><div class="v">'+segs.length+"</div></div>";
  html+='<div class="card"><div class="k">数据覆盖</div><div class="v">'+D.meta.days_covered+" 天</div></div>";
  var rr=D.recent7d_active_rate||{};var keys=Object.keys(rr);
  for(var i=0;i<keys.length;i++){var r=rr[keys[i]];var v=r.rate_pct_per_hour==null?"—":r.rate_pct_per_hour+" %/h";
    html+='<div class="card"><div class="k">近7天活跃耗速率 · '+esc(keys[i])+'</div><div class="v">'+v+"</div></div>";}
  s.innerHTML=html;
})();
if(!segs.length){emptyMark("c1");emptyMark("c2");emptyMark("c3");document.getElementById("c3legend").innerHTML="";
  document.querySelector("#seg tbody").innerHTML='<tr><td colspan="11" style="text-align:center;color:#888">暂无数据</td></tr>';
}else{
// ④ 分段明细表
(function(){
  var rows="";
  for(var i=0;i<segs.length;i++){var s=segs[i];
    rows+="<tr><td>"+esc(s.device_key)+"</td><td>"+esc(s.display_name)+"</td><td>"+s.segment_type+"</td><td>"+fmtTs(s.start_ts)+"</td><td>"+fmtTs(s.end_ts)+"</td><td>"+s.duration_min+"</td><td>"+s.start_pct+"</td><td>"+s.end_pct+"</td><td>"+s.delta_pct+"</td><td>"+(s.rate_pct_per_hour==null?"":s.rate_pct_per_hour.toFixed(2))+"</td><td>"+esc(s.note)+"</td></tr>";}
  document.querySelector("#seg tbody").innerHTML=rows;
})();
// ① 电量曲线：未知段断线 + 灰罩；单 polyline，禁逐点元素
(function(){
  var svg=document.getElementById("c1");var W=960,H=280,PL=36,PR=8,PT=10,PB=26;
  var maxTs=0,minTs=Infinity,i,j;
  for(i=0;i<segs.length;i++){if(segs[i].end_ts>maxTs)maxTs=segs[i].end_ts;if(segs[i].start_ts<minTs)minTs=segs[i].start_ts;}
  function draw(winDays){
    while(svg.firstChild)svg.removeChild(svg.firstChild);
    var lo=winDays>0?Math.max(minTs,maxTs-winDays*86400):minTs,hi=maxTs;
    if(hi<=lo)hi=lo+3600;
    var hours=(hi-lo)/3600;
    var w=Math.max(W,Math.min(9600,PL+PR+Math.ceil(hours*2)));
    svg.setAttribute("width",w);svg.setAttribute("height",H);
    function X(t){return PL+(t-lo)/(hi-lo)*(w-PL-PR);}
    function Y(p){return PT+(100-p)/100*(H-PT-PB);}
    // 背景区间：未知灰罩、充电底色
    for(i=0;i<D.chart1.length;i++){var sr=D.chart1[i];
      for(j=0;j<sr.unknown_spans.length;j++){var u=sr.unknown_spans[j];if(u[1]<lo||u[0]>hi)continue;
        el("rect",{x:X(Math.max(u[0],lo)),y:PT,width:Math.max(1,X(Math.min(u[1],hi))-X(Math.max(u[0],lo))),height:H-PT-PB,fill:"#a0aec0","fill-opacity":0.18},svg);}
      for(j=0;j<sr.charging_spans.length;j++){var c=sr.charging_spans[j];if(c[1]<lo||c[0]>hi)continue;
        el("rect",{x:X(Math.max(c[0],lo)),y:PT,width:Math.max(1,X(Math.min(c[1],hi))-X(Math.max(c[0],lo))),height:H-PT-PB,fill:"#2f855a","fill-opacity":0.10},svg);}}
    // 轴线与刻度（限量文本元素）
    el("line",{x1:PL,y1:Y(0),x2:w-PR,y2:Y(0),stroke:"#888","stroke-width":0.5},svg);
    el("line",{x1:PL,y1:PT,x2:PL,y2:H-PB,stroke:"#888","stroke-width":0.5},svg);
    [0,25,50,75,100].forEach(function(p){var t=el("text",{x:4,y:Y(p)+4,"font-size":10,fill:"#888"},svg);t.textContent=p;});
    var tickN=Math.min(12,Math.max(2,Math.floor(hours/24)+1));
    for(i=0;i<tickN;i++){var tt=lo+(hi-lo)*i/(tickN-1);var tx=el("text",{x:X(tt)-26,y:H-8,"font-size":10,fill:"#888"},svg);tx.textContent=fmtTs(tt).slice(0,10);}
    // 每设备折线：null 断点拆 run，每个 run 一条 polyline
    for(i=0;i<D.chart1.length;i++){var sr2=D.chart1[i];var col=PALETTE[i%PALETTE.length];var run=[];
      var flush=function(){if(run.length>1){el("polyline",{points:run.join(" "),fill:"none",stroke:col,"stroke-width":1.4},svg);}run=[];};
      for(j=0;j<sr2.points.length;j++){var pt=sr2.points[j];if(pt[0]<lo-3599||pt[0]>hi)continue;
        if(pt[1]==null){flush();}else{run.push(X(pt[0]+1800).toFixed(1)+","+Y(pt[1]).toFixed(1));}}
      flush();}
    // 图例
    for(i=0;i<D.chart1.length;i++){var lx=el("text",{x:PL+8+i*150,y:PT+12,"font-size":11,fill:PALETTE[i%PALETTE.length]},svg);lx.textContent=D.chart1[i].device_key+" "+D.chart1[i].display_name;}
  }
  draw(90);
  var btns=document.querySelectorAll("#c1ctl button");
  btns.forEach(function(b){b.addEventListener("click",function(){
    btns.forEach(function(x){x.className="";});b.className="on";draw(+b.getAttribute("data-win"));});});
})();
// ② 每日耗电对比（堆叠柱：活跃/休眠/未知）
(function(){
  var svg=document.getElementById("c2");var W=960,H=260,PL=36,PT=10,PB=40;
  var days=Object.keys(D.chart2||{});if(!days.length){emptyMark("c2");return;}
  var devKeys=(D.devices||[]).map(function(d){return d[0];});
  var maxV=0.0001;var sums={};
  days.forEach(function(d){var per=D.chart2[d];var a=0,s=0,u=0;
    devKeys.forEach(function(k){var c=per[k]||{};a+=c.active||0;s+=c.sleep||0;u+=c.unknown||0;});
    sums[d]={active:a,sleep:s,unknown:u};var t=a+s+u;if(t>maxV)maxV=t;});
  var bw=Math.max(8,Math.min(48,(W-PL-20)/days.length-6));
  days.forEach(function(d,i){
    var x=PL+((W-PL-20)/days.length)*i+3;var y=H-PB;var v=sums[d];
    ["active","sleep","unknown"].forEach(function(tp){
      var h=(v[tp]/maxV)*(H-PT-PB);if(h>0){el("rect",{x:x,y:y-h,width:bw,height:h,fill:TYPE_COLOR[tp],"fill-opacity":tp==="unknown"?0.55:0.85},svg);y-=h;}});
    if(days.length<=62||i%Math.ceil(days.length/62)===0){var t=el("text",{x:x+bw/2-14,y:H-PB+14,"font-size":9,fill:"#888"},svg);t.textContent=d.slice(5);}
  });
  el("line",{x1:PL,y1:H-PB,x2:W-8,y2:H-PB,stroke:"#888","stroke-width":0.5},svg);
  var lg=document.getElementById("c3legend");
  lg.innerHTML='<span><i style="background:#2b6cb0"></i>活跃</span><span><i style="background:#6b46c1"></i>休眠</span><span><i style="background:#2f855a"></i>充电</span><span><i style="background:#a0aec0"></i>未知</span>'
    +(D.meta.chart2_approx?' <span style="color:#b26a00">含跨日段线性摊入（近似）</span>':"");
})();
// ③ 时长占比（每设备一条堆叠横条 + 合计）
(function(){
  var svg=document.getElementById("c3");var W=960,PL=110,PT=14,RH=22,GAP=8;
  var rows=[];var pd=D.chart3.per_device||{};Object.keys(pd).forEach(function(k){rows.push([k,pd[k]]);});
  rows.push(["合计",D.chart3.total]);
  var H=PT+rows.length*(RH+GAP)+8;svg.setAttribute("height",H);
  rows.forEach(function(r,i){
    var k=r[0],v=r[1];var tot=(v.active||0)+(v.sleep||0)+(v.charging||0)+(v.unknown||0);
    var y=PT+i*(RH+GAP);var t=el("text",{x:4,y:y+RH-7,"font-size":11,fill:"#888"},svg);t.textContent=k;
    if(tot<=0){var e=el("text",{x:PL,y:y+RH-7,"font-size":11,fill:"#aaa"},svg);e.textContent="暂无数据";return;}
    var x=PL;
    ["active","sleep","charging","unknown"].forEach(function(tp){
      var w=(v[tp]||0)/tot*(W-PL-90);if(w>0){el("rect",{x:x,y:y,width:w,height:RH,fill:TYPE_COLOR[tp],"fill-opacity":tp==="unknown"?0.55:0.85},svg);x+=w;}});
    var pct=el("text",{x:x+6,y:y+RH-7,"font-size":10,fill:"#888"},svg);
    pct.textContent=Math.round((v.active||0)/tot*100)+"/"+Math.round((v.sleep||0)/tot*100)+"/"+Math.round((v.charging||0)/tot*100)+"/"+Math.round((v.unknown||0)/tot*100)+"%";
  });
})();
}
</script>
</body>
</html>
"##;

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::{BoundaryDevice, ReadEvent, SOURCE_POLL, SOURCE_WAKE_FIRST_READ};

    const CST: i32 = 480; // +08:00

    fn ev_read(ts: i64, key: &str, pct: u8, charging: bool, source: &str) -> StatsEvent {
        StatsEvent::Read(ReadEvent {
            ts,
            physical_key: key.to_string(),
            display_name: format!("Name-{key}"),
            percent: pct,
            charging,
            source: source.to_string(),
        })
    }

    fn ev_poll(ts: i64, key: &str, pct: u8) -> StatsEvent {
        ev_read(ts, key, pct, false, SOURCE_POLL)
    }

    fn seg(
        key: &str,
        t: SegmentType,
        start: i64,
        end: i64,
        sp: u8,
        ep: u8,
        notes: &[&str],
    ) -> Segment {
        Segment {
            device_key: key.to_string(),
            display_name: format!("Name-{key}"),
            seg_type: t,
            start_ts: start,
            end_ts: end,
            start_pct: sp,
            end_pct: ep,
            notes: notes.iter().map(|s| s.to_string()).collect(),
            merged: false,
        }
    }

    // -- 时间工具 ----------------------------------------------------------

    #[test]
    fn fmt_rfc3339_golden_values() {
        assert_eq!(fmt_rfc3339(0, CST), "1970-01-01T08:00:00+08:00");
        assert_eq!(fmt_rfc3339(2000, CST), "1970-01-01T08:33:20+08:00");
        assert_eq!(fmt_rfc3339(1_760_000_000, CST), "2025-10-09T16:53:20+08:00");
        assert_eq!(fmt_rfc3339(1_760_745_600, CST), "2025-10-18T08:00:00+08:00");
        // 零偏移与负偏移。
        assert_eq!(fmt_rfc3339(0, 0), "1970-01-01T00:00:00+00:00");
        assert_eq!(fmt_rfc3339(0, -300), "1969-12-31T19:00:00-05:00");
        // 非整小时偏移（如 +05:30）。
        assert_eq!(fmt_rfc3339(0, 330), "1970-01-01T05:30:00+05:30");
    }

    #[test]
    fn fmt_local_date_uses_injected_offset() {
        // UTC 2025-10-18T00:00:00：+08:00 下是 10-18 08:00，-05:00 下是 10-17。
        assert_eq!(fmt_local_date(1_760_745_600, CST), "2025-10-18");
        assert_eq!(fmt_local_date(1_760_745_600, -300), "2025-10-17");
        assert_eq!(local_day_start(1_760_745_600, 0), 1_760_745_600);
        assert_eq!(local_day_start(1_760_745_600 + 1, 0), 1_760_745_600);
    }

    // -- 分段引擎 ----------------------------------------------------------

    /// 复合场景：run 边界、活跃、休眠（sleep_enter 唯一起点）、充电、
    /// run_gap 未知、coalesce、进行中尾段不落段。
    #[test]
    fn segmentation_composite_scenario() {
        let k = "kbd-1";
        let events = vec![
            StatsEvent::RunStart { ts: 1000 },
            ev_poll(2000, k, 90),
            ev_poll(9200, k, 88),
            StatsEvent::SleepEnter {
                ts: 10000,
                physical_key: k.into(),
            },
            ev_read(24400, k, 85, false, SOURCE_WAKE_FIRST_READ),
            ev_read(28000, k, 87, true, SOURCE_POLL),
            ev_read(31600, k, 95, true, SOURCE_POLL),
            StatsEvent::RunStop { ts: 33400 },
            StatsEvent::RunStart { ts: 119800 },
            ev_poll(123400, k, 80),
        ];
        let segs = build_segments(&events);
        assert_eq!(segs.len(), 7, "segments: {segs:?}");
        // 1. run_start→首读：无在先读数证据 → 未知 evidence_missing，<1h 标 below_window。
        assert_eq!(segs[0].seg_type, SegmentType::Unknown);
        assert_eq!((segs[0].start_ts, segs[0].end_ts), (1000, 2000));
        assert!(segs[0].notes.contains(&NOTE_EVIDENCE_MISSING.to_string()));
        assert!(segs[0].notes.contains(&NOTE_BELOW_WINDOW.to_string()));
        // 2. 活跃 90→88（两个相邻活跃元素段已 coalesce：2000→10000）。
        assert_eq!(segs[1].seg_type, SegmentType::Active);
        assert_eq!((segs[1].start_ts, segs[1].end_ts), (2000, 10000));
        assert_eq!((segs[1].start_pct, segs[1].end_pct), (90, 88));
        assert_eq!(segs[1].rate_pct_per_hour(), Some(-0.9));
        // 3. 休眠 88→85（sleep_enter 起点 → 唤醒首读终点）。
        assert_eq!(segs[2].seg_type, SegmentType::Sleep);
        assert_eq!((segs[2].start_ts, segs[2].end_ts), (10000, 24400));
        assert_eq!((segs[2].start_pct, segs[2].end_pct), (88, 85));
        assert_eq!(segs[2].rate_pct_per_hour(), Some(-0.75));
        // 4. 活跃 85→87（充电刚开始，分类按段起点时最近已知 charging=false）。
        assert_eq!(segs[3].seg_type, SegmentType::Active);
        assert_eq!((segs[3].start_pct, segs[3].end_pct), (85, 87));
        // 5. 充电 87→95（coalesce 至 run_stop）。
        assert_eq!(segs[4].seg_type, SegmentType::Charging);
        assert_eq!((segs[4].start_ts, segs[4].end_ts), (28000, 33400));
        assert_eq!((segs[4].start_pct, segs[4].end_pct), (87, 95));
        // 6. 跨 run 缺口 → 未知 run_gap_unattributed。
        assert_eq!(segs[5].seg_type, SegmentType::Unknown);
        assert_eq!((segs[5].start_ts, segs[5].end_ts), (33400, 119800));
        assert_eq!(segs[5].note_text(), NOTE_RUN_GAP);
        assert_eq!(segs[5].rate_pct_per_hour(), None);
        // 7. 新 run 首段（分类沿用最后已知 charging=true）。
        assert_eq!(segs[6].seg_type, SegmentType::Charging);
        assert_eq!((segs[6].start_ts, segs[6].end_ts), (119800, 123400));
        assert_eq!((segs[6].start_pct, segs[6].end_pct), (95, 80));
        // 进行中尾段不落段：不存在以 123400 为起点的段。
        assert!(segs.iter().all(|s| s.start_ts != 123400));
        // 段首尾相接不重叠不遗漏（同设备内）。
        for w in segs.windows(2) {
            assert_eq!(w[0].end_ts, w[1].start_ts);
        }
    }

    #[test]
    fn clock_anomaly_and_gap_over_7d_marked_unknown() {
        let events = vec![
            // dev-a：时间倒流 Δt≤0。
            ev_poll(1000, "dev-a", 50),
            ev_poll(900, "dev-a", 49),
            // dev-b：Δt > 7 天。
            ev_poll(1000, "dev-b", 50),
            ev_poll(1000 + 8 * 86_400, "dev-b", 40),
        ];
        let segs = build_segments(&events);
        assert_eq!(segs.len(), 2);
        let a = &segs[0];
        assert_eq!(a.device_key, "dev-a");
        assert_eq!(a.seg_type, SegmentType::Unknown);
        assert!(a.notes.contains(&NOTE_CLOCK_ANOMALY.to_string()));
        // <1h 且无同型邻段 → below_window。
        assert!(a.notes.contains(&NOTE_BELOW_WINDOW.to_string()));
        assert_eq!(a.rate_pct_per_hour(), None);
        let b = &segs[1];
        assert_eq!(b.device_key, "dev-b");
        assert_eq!(b.seg_type, SegmentType::Unknown);
        assert!(b.notes.contains(&NOTE_GAP_OVER_7D.to_string()));
        assert!(!b.notes.contains(&NOTE_BELOW_WINDOW.to_string())); // ≥1h 不标
    }

    #[test]
    fn wake_first_read_without_sleep_enter_is_evidence_missing() {
        let events = vec![
            ev_poll(1000, "kbd-1", 90),
            ev_read(5000, "kbd-1", 88, false, SOURCE_WAKE_FIRST_READ),
        ];
        let segs = build_segments(&events);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].seg_type, SegmentType::Unknown);
        assert_eq!(segs[0].note_text(), NOTE_EVIDENCE_MISSING);
        assert_eq!((segs[0].start_pct, segs[0].end_pct), (90, 88));
    }

    #[test]
    fn sleep_state_persists_across_run_boundary() {
        // sleep_enter 后跨 run：缺口段标未知，run 内部分仍按休眠归因。
        let k = "kbd-1";
        let events = vec![
            ev_poll(500, k, 90),
            StatsEvent::SleepEnter {
                ts: 1000,
                physical_key: k.into(),
            },
            StatsEvent::RunStop { ts: 2000 },
            StatsEvent::RunStart { ts: 90000 },
            ev_read(95000, k, 88, false, SOURCE_WAKE_FIRST_READ),
        ];
        let segs = build_segments(&events);
        assert_eq!(segs.len(), 4, "{segs:?}");
        assert_eq!(segs[1].seg_type, SegmentType::Sleep); // 1000→2000
        assert_eq!(segs[2].seg_type, SegmentType::Unknown); // 2000→90000 run_gap
        assert_eq!(segs[2].note_text(), NOTE_RUN_GAP);
        assert_eq!(segs[3].seg_type, SegmentType::Sleep); // 90000→95000 仍休眠
        assert_eq!((segs[3].start_pct, segs[3].end_pct), (90, 88));
    }

    #[test]
    fn boundary_closes_segment_with_note_but_coalesce_keeps_type_flow() {
        let k = "kbd-1";
        let events = vec![
            ev_poll(1000, k, 90),
            StatsEvent::Boundary {
                ts: 8200,
                devices: vec![BoundaryDevice {
                    physical_key: k.into(),
                    percent: 85,
                    charging: false,
                }],
            },
            ev_poll(15400, k, 83),
        ];
        let segs = build_segments(&events);
        // 相邻同型 coalesce 为一段；boundary note 保留在合并段上。
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].seg_type, SegmentType::Active);
        assert_eq!((segs[0].start_ts, segs[0].end_ts), (1000, 15400));
        assert_eq!((segs[0].start_pct, segs[0].end_pct), (90, 83));
        assert!(segs[0].notes.contains(&NOTE_BOUNDARY.to_string()));
    }

    // -- ≥1h 确定性合并 ------------------------------------------------------

    #[test]
    fn merge_into_single_same_type_neighbor() {
        let mut segs = vec![
            seg("k", SegmentType::Active, 0, 36_000, 100, 90, &[]),
            seg("k", SegmentType::Active, 36_000, 37_800, 90, 89, &[]),
            seg("k", SegmentType::Sleep, 37_800, 55_800, 89, 87, &[]),
        ];
        merge_below_window(&mut segs);
        assert_eq!(segs.len(), 2);
        assert_eq!((segs[0].start_ts, segs[0].end_ts), (0, 37_800));
        assert_eq!((segs[0].start_pct, segs[0].end_pct), (100, 89));
        assert!(!segs[0].notes.contains(&NOTE_BELOW_WINDOW.to_string()));
        assert!(!segs[0].merged, "内部 merged 标记已复位");
    }

    #[test]
    fn merge_both_sides_picks_longer_then_tie_prev() {
        // 两侧均可并 → 并入时长较长一侧（next 3h > prev 2h）。
        let mut segs = vec![
            seg("k", SegmentType::Active, 0, 7_200, 100, 98, &[]),
            seg("k", SegmentType::Active, 7_200, 9_000, 98, 97, &[]),
            seg("k", SegmentType::Active, 9_000, 19_800, 97, 95, &[]),
        ];
        merge_below_window(&mut segs);
        assert_eq!(segs.len(), 2);
        assert_eq!((segs[1].start_ts, segs[1].end_ts), (7_200, 19_800));
        assert_eq!((segs[1].start_pct, segs[1].end_pct), (98, 95));
        // 等长 → 取先（prev）。
        let mut segs = vec![
            seg("k", SegmentType::Active, 0, 10_800, 100, 98, &[]),
            seg("k", SegmentType::Active, 10_800, 12_600, 98, 97, &[]),
            seg("k", SegmentType::Active, 12_600, 23_400, 97, 95, &[]),
        ];
        merge_below_window(&mut segs);
        assert_eq!(segs.len(), 2);
        assert_eq!((segs[0].start_ts, segs[0].end_ts), (0, 12_600));
        assert_eq!((segs[0].start_pct, segs[0].end_pct), (100, 97));
    }

    #[test]
    fn merge_no_same_type_neighbor_keeps_with_below_window() {
        let mut segs = vec![
            seg("k", SegmentType::Active, 0, 18_000, 100, 95, &[]),
            seg("k", SegmentType::Sleep, 18_000, 19_800, 95, 94, &[]),
            seg("k", SegmentType::Active, 19_800, 37_800, 94, 90, &[]),
        ];
        merge_below_window(&mut segs);
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[1].seg_type, SegmentType::Sleep);
        assert!(segs[1].notes.contains(&NOTE_BELOW_WINDOW.to_string()));
    }

    #[test]
    fn merge_result_still_below_window_gets_noted() {
        let mut segs = vec![
            seg("k", SegmentType::Active, 0, 1_440, 100, 99, &[]),
            seg("k", SegmentType::Active, 1_440, 2_520, 99, 98, &[]),
        ];
        merge_below_window(&mut segs);
        assert_eq!(segs.len(), 1);
        assert_eq!((segs[0].start_ts, segs[0].end_ts), (0, 2_520));
        assert!(segs[0].notes.contains(&NOTE_BELOW_WINDOW.to_string()));
    }

    #[test]
    fn merge_single_round_no_cascade() {
        // 中段并入 prev 后，prev 已参与合并 → 后段不得再并入（不级联）。
        let mut segs = vec![
            seg("k", SegmentType::Active, 0, 3_600, 100, 99, &[]),
            seg("k", SegmentType::Active, 3_600, 5_400, 99, 98, &[]),
            seg("k", SegmentType::Active, 5_400, 7_200, 98, 97, &[]),
        ];
        merge_below_window(&mut segs);
        assert_eq!(segs.len(), 2);
        assert_eq!((segs[0].start_ts, segs[0].end_ts), (0, 5_400));
        assert!(!segs[0].notes.contains(&NOTE_BELOW_WINDOW.to_string()));
        assert_eq!((segs[1].start_ts, segs[1].end_ts), (5_400, 7_200));
        assert!(segs[1].notes.contains(&NOTE_BELOW_WINDOW.to_string()));
    }

    // -- 匿名化（I-04）------------------------------------------------------

    #[test]
    fn fnv1a64_golden_vectors() {
        assert_eq!(fnv1a64_hex(""), "cbf29ce484222325");
        assert_eq!(fnv1a64_hex("a"), "af63dc4c8601ec8c");
        assert_eq!(fnv1a64_hex("foobar"), "85944171f73967e8");
    }

    #[test]
    fn anonymize_map_sorts_by_hash_lexicographic_not_key_order() {
        // fnv1a64("dev-1")=8596fc… < fnv1a64("dev-0")=8596fd…：
        // 键字典序 dev-0 < dev-1，但编号按哈希字典序 → dev-1 得 D1。
        let keys: BTreeSet<String> = ["dev-0", "dev-1", "kbd-1"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let m = anonymize_map(&keys);
        assert_eq!(m.get("dev-1").map(String::as_str), Some("D1"));
        assert_eq!(m.get("dev-0").map(String::as_str), Some("D2"));
        assert_eq!(m.get("kbd-1").map(String::as_str), Some("D3"));
        // 确定性：重复调用结果一致。
        assert_eq!(m, anonymize_map(&keys));
    }

    // -- CSV 导出 ------------------------------------------------------------

    #[test]
    fn csv_field_prefix_guard_formula_injection() {
        // E12：`+`/`-` 开头字段加 `'` 引前缀（表格软件按纯文本处理）。
        assert_eq!(csv_field("+cmd"), "'+cmd");
        assert_eq!(csv_field("-5%"), "'-5%");
        // 引前缀与引号包裹的组合：含逗号的 `+` 开头字段。
        assert_eq!(csv_field("+a,b"), "\"'+a,b\"");
        // 收窄口径：`=`/`@` 不加前缀（sanitize 已保证其到不了导出侧，
        // 此处仅验证 csv_field 本身不对它们加前缀）。
        assert_eq!(csv_field("=1+1"), "=1+1");
        assert_eq!(csv_field("@x"), "@x");
        // 普通字段不变；`+` 不在开头不触发。
        assert_eq!(csv_field("MX Master 3+"), "MX Master 3+");
        assert_eq!(csv_field(""), "");
    }

    #[test]
    fn csv_golden_bom_header_escaping_and_unknown_rate_blank() {
        let segs = vec![
            Segment {
                device_key: "kbd-1".into(),
                display_name: "My \"K380\", Keys".into(),
                ..seg("kbd-1", SegmentType::Active, 2000, 9200, 90, 88, &[])
            },
            seg(
                "kbd-1",
                SegmentType::Unknown,
                10000,
                24400,
                88,
                85,
                &[NOTE_EVIDENCE_MISSING],
            ),
        ];
        let csv = render_csv(&segs, None, CST);
        let mut lines = csv.lines();
        let first = lines.next().unwrap();
        assert!(first.starts_with('\u{feff}'), "BOM 缺失");
        assert_eq!(&first[3..], CSV_HEADER);
        assert_eq!(
            lines.next().unwrap(),
            "kbd-1,\"My \"\"K380\"\", Keys\",active,1970-01-01T08:33:20+08:00,1970-01-01T10:33:20+08:00,120,90,88,-2,-1.00,"
        );
        assert_eq!(
            lines.next().unwrap(),
            "kbd-1,Name-kbd-1,unknown,1970-01-01T10:46:40+08:00,1970-01-01T14:46:40+08:00,240,88,85,-3,,evidence_missing"
        );
        assert!(lines.next().is_none());
    }

    #[test]
    fn csv_empty_state_is_header_only_and_rows_sorted_by_display_key() {
        // 空态：BOM + 表头。
        let csv = render_csv(&[], None, CST);
        assert_eq!(csv, format!("\u{feff}{CSV_HEADER}\n"));
        // 行序 = 展示键字典序（匿名化后按 D 编号，哈希序 dev-1(D1) 先于 dev-0(D2)）。
        let segs = vec![
            seg("dev-0", SegmentType::Active, 0, 3600, 100, 99, &[]),
            seg("dev-1", SegmentType::Active, 0, 3600, 100, 99, &[]),
        ];
        let keys: BTreeSet<String> = ["dev-0", "dev-1"].iter().map(|s| s.to_string()).collect();
        let anon = anonymize_map(&keys);
        let csv = render_csv(&segs, Some(&anon), CST);
        let rows: Vec<&str> = csv.lines().skip(1).collect();
        assert_eq!(rows.len(), 2);
        assert!(rows[0].starts_with("D1,"), "row0: {}", rows[0]);
        assert!(rows[1].starts_with("D2,"), "row1: {}", rows[1]);
        // 键列（行首）不得出现原键（display_name 保留真实值属预期）。
        assert!(
            !csv.lines().any(|l| l.starts_with("dev-")),
            "匿名化后键列不得含原键"
        );
    }

    // -- JSON 导出（I-07）-----------------------------------------------------

    #[test]
    fn json_export_anonymizes_physical_key_and_preserves_names() {
        let events = vec![
            ev_poll(1000, "kbd-1", 90),
            StatsEvent::SleepEnter {
                ts: 2000,
                physical_key: "kbd-1".into(),
            },
            StatsEvent::Boundary {
                ts: 3000,
                devices: vec![
                    BoundaryDevice {
                        physical_key: "kbd-1".into(),
                        percent: 88,
                        charging: false,
                    },
                    BoundaryDevice {
                        physical_key: "mouse-2".into(),
                        percent: 60,
                        charging: true,
                    },
                ],
            },
            StatsEvent::RunStop { ts: 4000 },
        ];
        let anon = anonymize_map(&event_keys(&events));
        let v: serde_json::Value =
            serde_json::from_str(&render_json_events(&events, Some(&anon), 9999)).unwrap();
        assert_eq!(v["version"], 1);
        assert_eq!(v["exported_at"], 9999);
        let arr = v["events"].as_array().unwrap();
        assert_eq!(arr.len(), 4);
        let k = anon["kbd-1"].clone();
        let m = anon["mouse-2"].clone();
        assert_eq!(arr[0]["physical_key"], k);
        assert_eq!(
            arr[0]["display_name"], "Name-kbd-1",
            "display_name 保留真实值"
        );
        assert_eq!(arr[0]["source"], SOURCE_POLL);
        assert_eq!(arr[1]["physical_key"], k);
        assert_eq!(arr[1]["event"], "sleep_enter");
        assert_eq!(arr[2]["devices"][0]["physical_key"], k);
        assert_eq!(arr[2]["devices"][1]["physical_key"], m);
        assert_eq!(arr[3]["event"], "run_stop");
        let s = render_json_events(&events, Some(&anon), 1);
        // physical_key 字段值不得为原键（display_name 保留真实值属预期）。
        assert!(
            !s.contains("\"physical_key\":\"kbd-1\""),
            "read 事件键未脱敏"
        );
        assert!(
            !s.contains("\"physical_key\":\"mouse-2\""),
            "boundary 设备键未脱敏"
        );
        // 空态：events: []。
        let empty: serde_json::Value =
            serde_json::from_str(&render_json_events(&[], None, 1)).unwrap();
        assert_eq!(empty["events"].as_array().unwrap().len(), 0);
        // 未开匿名化：原键保留。
        let plain: serde_json::Value =
            serde_json::from_str(&render_json_events(&events, None, 1)).unwrap();
        assert_eq!(plain["events"][0]["physical_key"], "kbd-1");
    }

    // -- 报告数据块（AC-B8 对账源）--------------------------------------------

    #[test]
    fn report_data_matches_csv_and_chart_totals() {
        let k = "kbd-1";
        let events = vec![
            StatsEvent::RunStart { ts: 1000 },
            ev_poll(2000, k, 90),
            ev_poll(9200, k, 88),
            StatsEvent::SleepEnter {
                ts: 10000,
                physical_key: k.into(),
            },
            ev_read(24400, k, 85, false, SOURCE_WAKE_FIRST_READ),
            ev_read(28000, k, 87, true, SOURCE_POLL),
            ev_read(31600, k, 95, true, SOURCE_POLL),
            StatsEvent::RunStop { ts: 33400 },
            StatsEvent::RunStart { ts: 119800 },
            ev_poll(123400, k, 80),
        ];
        let segs = build_segments(&events);
        let report = build_report_data(&events, &segs, None, CST, 200000);
        // 分段明细行数 = CSV 数据行数 = 段数。
        let rows = report["segments"].as_array().unwrap();
        assert_eq!(rows.len(), segs.len());
        let csv_rows = render_csv(&segs, None, CST).lines().count() - 1;
        assert_eq!(rows.len(), csv_rows);
        // 行字段与 Segment 派生值一致。
        for (r, s) in rows.iter().zip(segs.iter()) {
            assert_eq!(r["device_key"].as_str().unwrap(), s.device_key);
            assert_eq!(r["segment_type"].as_str().unwrap(), s.seg_type.as_str());
            assert_eq!(r["start_ts"].as_i64().unwrap(), s.start_ts);
            assert_eq!(r["duration_min"].as_i64().unwrap(), s.duration_min());
            assert_eq!(r["delta_pct"].as_i64().unwrap(), s.delta_pct() as i64);
            match s.rate_pct_per_hour() {
                Some(rate) => assert_eq!(r["rate_pct_per_hour"].as_f64().unwrap(), rate),
                None => assert!(r["rate_pct_per_hour"].is_null()),
            }
        }
        // chart3 合计 = 段时长按类型求和。
        let mut exp = TypeDurations::default();
        for s in &segs {
            let d = s.duration_secs().max(0);
            match s.seg_type {
                SegmentType::Active => exp.active += d,
                SegmentType::Sleep => exp.sleep += d,
                SegmentType::Charging => exp.charging += d,
                SegmentType::Unknown => exp.unknown += d,
            }
        }
        let total = &report["chart3"]["total"];
        assert_eq!(total["active"].as_i64().unwrap(), exp.active);
        assert_eq!(total["sleep"].as_i64().unwrap(), exp.sleep);
        assert_eq!(total["charging"].as_i64().unwrap(), exp.charging);
        assert_eq!(total["unknown"].as_i64().unwrap(), exp.unknown);
        // chart2：跨日段线性摊入标记 + 数值（offset=+08:00，日界 ts=57600/144000）。
        assert!(report["meta"]["chart2_approx"].as_bool().unwrap());
        let day0 = &report["chart2"]["1970-01-01"][k];
        assert_eq!(day0["active"].as_f64().unwrap(), 2.0);
        assert_eq!(day0["sleep"].as_f64().unwrap(), 3.0);
        // US-B4：近 7 天活跃耗速率 = Σdrain/Σhours = 2 / (8000/3600 + 1) ≈ 0.62。
        let rr = &report["recent7d_active_rate"][k];
        assert_eq!(rr["rate_pct_per_hour"].as_f64().unwrap(), 0.62);
        assert_eq!(rr["days_covered"].as_i64().unwrap(), 2);
        assert_eq!(
            report["meta"]["sparse_note"].as_str().unwrap(),
            "数据不足 7 天，按实际 2 天统计"
        );
        // 设备清单与 meta。
        assert_eq!(report["devices"].as_array().unwrap().len(), 1);
        assert!(!report["meta"]["anonymized"].as_bool().unwrap());
        assert_eq!(report["meta"]["tz_offset"].as_str().unwrap(), "+08:00");
        assert_eq!(
            report["meta"]["generated_at_local"].as_str().unwrap(),
            fmt_rfc3339(200000, CST)
        );
        // chart1：桶覆盖首末段；未知段区间为灰罩源。
        let c1 = &report["chart1"][0];
        assert_eq!(c1["device_key"].as_str().unwrap(), k);
        let points = c1["points"].as_array().unwrap();
        assert_eq!(points[0][0].as_i64().unwrap(), 0); // min_ts=1000 → bucket 0
        assert_eq!(points[1][1].as_u64().unwrap(), 88); // bucket 1 末值 = 活跃段 end_pct
        let spans = c1["unknown_spans"].as_array().unwrap();
        assert_eq!(spans.len(), 2); // evidence_missing 段 + run_gap 段
        assert_eq!(c1["charging_spans"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn report_data_anonymized_hides_physical_keys() {
        let events = vec![ev_poll(1000, "kbd-1", 90), ev_poll(8200, "kbd-1", 88)];
        let segs = build_segments(&events);
        let anon = anonymize_map(&event_keys(&events));
        let report = build_report_data(&events, &segs, Some(&anon), CST, 9000);
        let s = report.to_string();
        assert!(report["meta"]["anonymized"].as_bool().unwrap());
        assert_eq!(report["segments"][0]["device_key"].as_str().unwrap(), "D1");
        assert_eq!(report["devices"][0][0].as_str().unwrap(), "D1");
        assert!(!s.contains("\"kbd-1\""), "数据块不得含 physical_key 明文");
        assert!(s.contains("Name-kbd-1"), "display_name 保留");
    }

    // -- HTML 报告静态核验（AC-B7 断网零请求）--------------------------------

    #[test]
    fn html_self_contained_no_external_requests() {
        let events = vec![
            ev_read(1000, "kbd-1", 90, false, SOURCE_POLL),
            ev_read(8200, "kbd-1", 88, false, SOURCE_POLL),
        ];
        let mut ev2 = events.clone();
        // 显示名以最后一条读取为准 → 改最后一条，让恶意串进入数据块。
        if let StatsEvent::Read(r) = ev2.last_mut().unwrap() {
            r.display_name = "x</script><script>alert(1)".into();
        }
        let segs = build_segments(&ev2);
        let report = build_report_data(&ev2, &segs, None, CST, 9000);
        let html = render_html(&report);
        // 零外部资源引用（SVG 命名空间 URI 是标识符、不产生请求，不计）。
        for pat in [
            "<script src",
            "<link",
            "<img",
            "@import",
            "url(",
            "fetch(",
            "XMLHttpRequest",
        ] {
            assert!(!html.contains(pat), "HTML 含外部请求模式: {pat}");
        }
        // 数据块存在且为唯一数据注入点。
        let tag = r#"<script id="volttray-data" type="application/json">"#;
        assert!(html.contains(tag));
        let after = html.split(tag).nth(1).unwrap();
        let block = after.split("</script>").next().unwrap();
        // 恶意显示名中的 </script> 已被  转义：数据块可完整解析。
        let parsed: serde_json::Value = serde_json::from_str(block).unwrap();
        assert_eq!(
            parsed["devices"][0][1].as_str().unwrap(),
            "x</script><script>alert(1)"
        );
        assert!(!block.contains("</script>"));
        // 空态定义存在。
        assert!(html.contains("暂无数据"));
    }

    // -- 时钟回拨负时出口径（质检 F1 钉死）--------------------------------

    #[test]
    fn clock_rewind_tail_negative_duration_pinned() {
        // 回拨尾段（质检最小复现）：run_start → read@2000 → read@9000 →
        // read@8900（时钟回拨 100s）。钉死口径：回拨段保留实际 ts 区间
        //（end<start）作异常证据，不钳位；duration_min 负值向下取整
        //（floor）——-100s → -2，与 recheck.py `(dur+30)//60` 逐值一致。
        let events = vec![
            StatsEvent::RunStart { ts: 0 },
            ev_poll(2000, "kbd-1", 90),
            ev_poll(9000, "kbd-1", 88),
            ev_poll(8900, "kbd-1", 89),
        ];
        let segs = build_segments(&events);
        let anomaly: Vec<&Segment> = segs
            .iter()
            .filter(|s| s.notes.iter().any(|n| n == NOTE_CLOCK_ANOMALY))
            .collect();
        assert_eq!(anomaly.len(), 1, "回拨段唯一: {segs:?}");
        let a = anomaly[0];
        assert_eq!(a.seg_type, SegmentType::Unknown);
        assert_eq!((a.start_ts, a.end_ts), (9000, 8900), "倒置段保留实际区间");
        assert_eq!(a.duration_secs(), -100);
        assert_eq!(a.duration_min(), -2, "负时长 floor：-100s → -2 分钟");
        assert_eq!(a.rate_pct_per_hour(), None, "unknown 段不产生速率");
        // 除钉死的回拨证据段外，其余段不得倒置（AC-B1 正常场景不破）。
        for s in &segs {
            if !s.notes.iter().any(|n| n == NOTE_CLOCK_ANOMALY) {
                assert!(s.end_ts >= s.start_ts, "非回拨段不得倒置: {s:?}");
            }
        }
    }

    #[test]
    fn coalesce_never_merges_inverted_segments() {
        // coalesce 口径（质检 F1 根因②钉死）：连续两条回拨段（同型
        // unknown 相邻）不得合并——旧实现无条件并入后段 end，产出
        // end<start 的段（9000→8800）。钉死：倒置段不参与合并、独立
        // 保留；coalesce 绝不产出 end_ts < start_ts 的段。
        let read = |ts, pct| Anchor {
            ts,
            kind: AnchorKind::Read,
            pct: Some(pct),
            charging: Some(false),
            wake_first_read: false,
        };
        let anchors = [
            read(2000, 90),
            read(9000, 88),
            read(8900, 89),
            read(8800, 90),
        ];
        let segs = segment_device("kbd-1", &anchors, "Name-kbd-1");
        assert_eq!(segs.len(), 3, "倒置段不参与 coalesce: {segs:?}");
        assert_eq!(segs[0].seg_type, SegmentType::Active);
        assert_eq!((segs[1].start_ts, segs[1].end_ts), (9000, 8900));
        assert_eq!((segs[2].start_ts, segs[2].end_ts), (8900, 8800));
        assert!(segs[1].notes.contains(&NOTE_CLOCK_ANOMALY.to_string()));
        assert!(segs[2].notes.contains(&NOTE_CLOCK_ANOMALY.to_string()));
        // 正常时序邻接的同型段仍合并（既有行为不回归）：连续正常
        // unknown（evidence_missing + 零时长 clock_anomaly）并为一段。
        let anchors2 = [
            read(2000, 90),
            read(9000, 88),
            read(9000, 87), // dt=0：零时长 clock_anomaly（非倒置）
        ];
        let segs2 = segment_device("kbd-1", &anchors2, "Name-kbd-1");
        // [2000→9000] active + [9000→9000] unknown（零时长非倒置，可与
        // 同型正常邻段合并；此处无同型邻段故独立）。
        assert_eq!(segs2.len(), 2, "零时长段不阻断正常流: {segs2:?}");
        assert_eq!((segs2[1].start_ts, segs2[1].end_ts), (9000, 9000));
    }

    // -- AC-B7 规模：双文件合计 ≥20MB 事件量的归因+渲染管道 --------------

    #[test]
    fn pipeline_at_dual_file_full_scale() {
        // 规模口径（AC-B7 双文件灌满 ≥20MB；质检 F3 修复钉死）：2 设备 ×
        // 270 天 × 每 2 分钟一条读取 = 195,480 事件。规模不再靠估算注释
        // ——测试内实测 jsonl 序列化字节数并断言 ≥20 MiB（20×1024×1024）。
        const DAYS: i64 = 270;
        let mut events = Vec::new();
        let base = 1_700_000_000i64;
        for dev in 0..2 {
            let key = format!("dev-{dev}");
            let mut ts = base;
            let mut pct = 100u8;
            for _day in 0..DAYS {
                for i in 0..360 {
                    // 白天 12h 活跃，每 2 分钟一条，约 30 分钟掉 1%。
                    if i % 15 == 0 && pct > 5 {
                        pct -= 1;
                    }
                    events.push(ev_read(ts, &key, pct, false, SOURCE_POLL));
                    ts += 120;
                }
                events.push(StatsEvent::SleepEnter {
                    ts,
                    physical_key: key.clone(),
                });
                ts += 11 * 3600; // 夜间休眠 11h
                pct = pct.saturating_sub(1);
                events.push(ev_read(ts, &key, pct, false, SOURCE_WAKE_FIRST_READ));
                ts += 3600;
            }
        }
        assert_eq!(events.len(), (2 * DAYS * 362) as usize, "事件规模");
        // AC-B7 规模实测：事件序列化为 jsonl 的字节合计 ≥20 MiB。
        let jsonl_bytes: usize = events.iter().map(|e| e.to_json_line().len() + 1).sum();
        eprintln!(
            "AC-B7 规模实测: {} 事件, jsonl {} B = {:.2} MiB",
            events.len(),
            jsonl_bytes,
            jsonl_bytes as f64 / 1048576.0
        );
        assert!(
            jsonl_bytes >= 20 * 1024 * 1024,
            "序列化规模 {jsonl_bytes}B 不足 20MiB（AC-B7）"
        );
        let segs = build_segments(&events);
        assert!(!segs.is_empty());
        // 段首尾相接（每设备独立校验）。
        for dev in 0..2 {
            let key = format!("dev-{dev}");
            let ds: Vec<&Segment> = segs.iter().filter(|s| s.device_key == key).collect();
            for w in ds.windows(2) {
                assert_eq!(w[0].end_ts, w[1].start_ts, "段链断裂: {key}");
            }
        }
        let csv = render_csv(&segs, None, CST);
        assert_eq!(csv.lines().count() - 1, segs.len(), "CSV 行数 = 段数");
        let report = build_report_data(&events, &segs, None, CST, base + DAYS * 86_400);
        assert_eq!(report["segments"].as_array().unwrap().len(), segs.len());
        let html = render_html(&report);
        assert!(html.contains(r#"<script id="volttray-data" type="application/json">"#));
        assert!(html.len() > 10_000);
        // 段数应远小于事件数（coalesce 生效），量级合理。
        assert!(segs.len() < events.len() / 10, "段数 {}", segs.len());
    }
}

#[cfg(test)]
mod filename_tests {
    use super::*;
    use crate::diag_export::LocalTimeParts;

    #[test]
    fn stats_export_filename_second_precision() {
        let p = LocalTimeParts {
            year: 2026,
            month: 10,
            day: 18,
            hour: 9,
            minute: 5,
            second: 3,
            millis: 777,
        };
        // R5-P-02：秒级、无毫秒、无冒号；millis 不参与。
        assert_eq!(
            stats_export_filename_local(p),
            "volttray-stats-20261018-090503.csv"
        );
    }
}
