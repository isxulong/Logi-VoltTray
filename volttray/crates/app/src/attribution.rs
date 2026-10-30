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

/// 速率列的最小段时长（秒）：短于此的段分母过小、速率失真
/// （融合稿口径总表「分段明细表」行：duration<5min → None；
/// 小修批第 4 项 60s 护栏在本批加严为 300s）。
pub const MIN_RATE_DURATION_SECS: i64 = 300;
/// 速率列的最小绝对 Δ%（融合稿口径总表：delta 绝对值<1% → None；
/// 整数电量口径下即 delta==0 的段不输出速率）。
pub const MIN_RATE_ABS_DELTA_PCT: i16 = 1;

/// 段类型（CSV segment_type 列取值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum SegmentType {
    /// 口径（小修批第 5 项文档化，2026-10-25 批次）：`active` 指
    /// 「app 运行中且未判充电/休眠」的段，**不是**「鼠标/用户活动
    /// 中」——真机 v0.3.23 分析已勘误，活动强弱的正确口径是 Raw
    /// Input 事件桶而非本枚举。命名保留 `active` 不改：CSV/JSON
    /// schema 已对外稳定（recheck.py 质检、E10 对账均按此列名取值），
    /// 改名会破坏既有对账；若未来 HTML 报告重构需要用户向措辞，
    /// 由报告层映射展示文案、不动本枚举与序列化值。
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
    /// 小修批第 4 项（2026-10-25 批次）：时长过短的段分母过小，
    /// 速率失真无意义（v0.3.23 真机末行 9s 段算出 -400%/h），统一
    /// 返回 None——CSV / JSON / HTML 同走本函数，护栏单点生效。
    /// 融合稿口径总表加严：duration<5min 或 delta 绝对值<1% → None
    /// （零变化段无速率意义，同时消灭小分母伪值）。
    pub fn rate_pct_per_hour(&self) -> Option<f64> {
        if self.seg_type == SegmentType::Unknown
            || self.duration_secs() < MIN_RATE_DURATION_SECS
            || self.delta_pct().abs() < MIN_RATE_ABS_DELTA_PCT
        {
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
            // 活动桶摘要不参与分段锚定（分钟位图是独立口径，
            // 分段引擎只消费 read/run/boundary/sleep 四类）。
            StatsEvent::ActivityBuckets { .. } => {}
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
            StatsEvent::SleepEnter { physical_key, .. }
            | StatsEvent::ActivityBuckets { physical_key, .. } => {
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

/// 粘滞段时长下限（秒）：斜率 0 且超过此时长的放电段判「设备上报
/// 粘滞」（E21 定论：86% 恒读 110.7h 平台期为设备侧上报粘滞）。
pub const STICKY_MIN_DURATION_SECS: i64 = 86_400;
/// 日卡「样本不足」护栏：当日 read 点数下限（口径总表：read<3 → 灰态）。
pub const SPARSE_MIN_READS: usize = 3;
/// 日卡「样本不足」护栏：当日覆盖时长下限（口径总表：覆盖<2h → 灰态）。
pub const SPARSE_MIN_COVERAGE_SECS: i64 = 7200;
/// 分日视图降级阈值：窗口超过 90 天时日卡降级为周聚合、时间轴抽稀
/// （融合稿 §8；写死进代码常量，经 meta.daily_view_day_limit 透出）。
pub const DAILY_VIEW_DAY_LIMIT: i64 = 90;
/// 双口径对账冲突阈值：上界（中位数×24h）与下界（总账）差异 >2 倍
/// 时挂「口径冲突」提示（融合稿冲突清单 1；区间形态不变）。
pub const AVG_CONFLICT_RATIO: f64 = 2.0;
/// 双口径对账冲突绝对阈值（FR-3 u=0 退化分支）：u == 0 时比值式
/// 判据失效，退化为总账绝对阈值（l > 1.0 %/天 才置真）。
pub const AVG_CONFLICT_ABS_THRESHOLD_PCT: f64 = 1.0;
/// 样本质量徽标阈值：有效日 <7 天挂「样本不足」（融合稿 §3③）。
pub const AVG_MIN_VALID_DAYS: usize = 7;
/// 周环比窗口日历缺口阈值：窗口内相邻两日卡日期差 >2 天时挂
/// 「窗口含缺口」质量徽标（FR-5 / 冲突 C-4：不 suppress、照常给 delta）。
pub const WOW_GAP_THRESHOLD_DAYS: i64 = 2;

/// 数据覆盖的本地日集合（段覆盖去重日；日界 = 本地 00:00）。
fn covered_days(segments: &[Segment], offset_minutes: i32) -> BTreeSet<String> {
    let mut days = BTreeSet::new();
    for s in segments {
        let mut t = s.start_ts;
        while t < s.end_ts.max(s.start_ts + 1) && t <= s.end_ts {
            days.insert(fmt_local_date(t, offset_minutes));
            let next = local_day_start(t, offset_minutes) + 86_400;
            if next <= t {
                break;
            }
            t = next;
        }
    }
    days
}

/// 粘滞段清单（per device）：Active/Sleep 放电段、Δ%=0、时长 >24h。
fn sticky_spans(segments: &[Segment]) -> BTreeMap<String, Vec<(i64, i64)>> {
    let mut out: BTreeMap<String, Vec<(i64, i64)>> = BTreeMap::new();
    for s in segments {
        if matches!(s.seg_type, SegmentType::Active | SegmentType::Sleep)
            && s.start_pct == s.end_pct
            && s.duration_secs() > STICKY_MIN_DURATION_SECS
        {
            out.entry(s.device_key.clone())
                .or_default()
                .push((s.start_ts, s.end_ts));
        }
    }
    out
}

/// 粘滞段覆盖的本地日集合（per device；与「中位数样本剔除」同源）。
fn sticky_day_set(
    spans: &BTreeMap<String, Vec<(i64, i64)>>,
    offset_minutes: i32,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (dev, list) in spans {
        for (a, b) in list {
            let mut t = *a;
            loop {
                out.entry(dev.clone())
                    .or_default()
                    .insert(fmt_local_date(t, offset_minutes));
                if t >= *b {
                    break;
                }
                let next = local_day_start(t, offset_minutes) + 86_400;
                if next <= t {
                    break;
                }
                t = next.min(*b);
            }
        }
    }
    out
}

/// 分日视图日卡（融合稿指标①②：日耗电 = 当日放电快照首末差，
/// 跨日段在日界处切分、不摊入，charging-fast 剔除；两级质量标签
/// 「设备上报粘滞 / 样本不足」）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DailyCard {
    pub date: String,
    pub first_ts: i64,
    pub first_pct: u8,
    pub last_ts: i64,
    pub last_pct: u8,
    /// 日耗电 = max(0, first−last)（电量回升不产生负耗电）。
    pub drain_pct: i16,
    /// 当日充电回补（%）：充电段增益按段中点归属日累加，跨日段不摊。
    pub charge_recovery_pct: i16,
    pub reads: usize,
    pub coverage_secs: i64,
    /// 当日放电实测斜率（%/h）；样本不足时 None（只给首末差）。
    pub slope_pct_per_hour: Option<f64>,
    /// 设备上报粘滞标签（当日落在 >24h 零斜率放电段内）。
    pub sticky: bool,
    /// 样本不足标签（read<3 或覆盖<2h）。
    pub sparse: bool,
    /// 当日充电段（日界裁剪后）。
    pub charging_spans: Vec<(i64, i64)>,
    /// 当日未知段（日界裁剪后，灰罩）。
    pub unknown_spans: Vec<(i64, i64)>,
}

fn build_daily_cards(
    events: &[StatsEvent],
    segments: &[Segment],
    sticky_days: &BTreeMap<String, BTreeSet<String>>,
    offset_minutes: i32,
) -> BTreeMap<String, Vec<DailyCard>> {
    // 当日放电快照（剔除充电态与 charging-fast 事件还原段）。
    let mut reads: BTreeMap<(String, String), Vec<(i64, u8)>> = BTreeMap::new();
    for ev in events {
        if let StatsEvent::Read(e) = ev {
            if e.charging || e.source == crate::stats::SOURCE_CHARGING_FAST {
                continue;
            }
            reads
                .entry((e.physical_key.clone(), fmt_local_date(e.ts, offset_minutes)))
                .or_default()
                .push((e.ts, e.percent));
        }
    }
    // 日界裁剪的充电/未知段。
    type DaySpans = (Vec<(i64, i64)>, Vec<(i64, i64)>);
    let mut spans: BTreeMap<(String, String), DaySpans> = BTreeMap::new();
    for seg in segments {
        if !matches!(seg.seg_type, SegmentType::Charging | SegmentType::Unknown) {
            continue;
        }
        let mut t = seg.start_ts;
        while t < seg.end_ts.max(seg.start_ts + 1) && t <= seg.end_ts {
            let d0 = local_day_start(t, offset_minutes);
            let clip = (seg.start_ts.max(d0), seg.end_ts.min(d0 + 86_400));
            if clip.1 > clip.0 {
                let cell = spans
                    .entry((seg.device_key.clone(), fmt_local_date(t, offset_minutes)))
                    .or_default();
                match seg.seg_type {
                    SegmentType::Charging => cell.0.push(clip),
                    _ => cell.1.push(clip),
                }
            }
            let next = d0 + 86_400;
            if next <= t {
                break;
            }
            t = next;
        }
    }
    // 当日充电回补（按充电段中点归属日；段增益 max(0, end−start)）。
    let mut recovery: BTreeMap<(String, String), i16> = BTreeMap::new();
    for seg in segments {
        if seg.seg_type != SegmentType::Charging {
            continue;
        }
        let gain = (seg.end_pct as i16 - seg.start_pct as i16).max(0);
        if gain == 0 {
            continue;
        }
        let mid = seg.start_ts + seg.duration_secs() / 2;
        *recovery
            .entry((seg.device_key.clone(), fmt_local_date(mid, offset_minutes)))
            .or_insert(0) += gain;
    }
    let mut out: BTreeMap<String, Vec<DailyCard>> = BTreeMap::new();
    for ((dev, date), mut list) in reads {
        list.sort_by_key(|(ts, _)| *ts);
        let (first_ts, first_pct) = list[0];
        let (last_ts, last_pct) = list[list.len() - 1];
        let coverage = last_ts - first_ts;
        let sparse = list.len() < SPARSE_MIN_READS || coverage < SPARSE_MIN_COVERAGE_SECS;
        let slope = if !sparse && coverage > 0 {
            Some(round2(
                (first_pct as f64 - last_pct as f64) / (coverage as f64 / 3600.0),
            ))
        } else {
            None
        };
        let sticky = sticky_days.get(&dev).is_some_and(|ds| ds.contains(&date));
        let (charging_spans, unknown_spans) = spans
            .get(&(dev.clone(), date.clone()))
            .cloned()
            .unwrap_or_default();
        let charge_recovery_pct = recovery
            .get(&(dev.clone(), date.clone()))
            .copied()
            .unwrap_or(0);
        out.entry(dev).or_default().push(DailyCard {
            date,
            first_ts,
            first_pct,
            last_ts,
            last_pct,
            drain_pct: (first_pct as i16 - last_pct as i16).max(0),
            charge_recovery_pct,
            reads: list.len(),
            coverage_secs: coverage,
            slope_pct_per_hour: slope,
            sticky,
            sparse,
            charging_spans,
            unknown_spans,
        });
    }
    for cards in out.values_mut() {
        cards.sort_by(|a, b| a.date.cmp(&b.date));
    }
    out
}

/// 每日 app 运行时段（run_start/run_stop 配对，日界裁剪；全局口径
/// ——app 进程级事件，各设备分日时间轴共用）。末尾未闭合 run 以
/// 本会话最后事件 ts 闭合；RunStart 覆盖未闭合 run（崩溃+重启）时，
/// 先以「上一会话最后一个事件 ts」闭合上一段，open 之后无任何事件
/// 则丢弃不开段（FR-4：闭合到上一会话末尾，不虚计停机时间）。
fn build_run_spans_by_day(
    events: &[StatsEvent],
    offset_minutes: i32,
) -> BTreeMap<String, Vec<(i64, i64)>> {
    let mut spans: Vec<(i64, i64)> = Vec::new();
    let mut open: Option<i64> = None;
    // open 之后最近一次事件的 ts（崩溃段闭合点取证）。
    let mut last_event_after_open: Option<i64> = None;
    for ev in events {
        match ev {
            StatsEvent::RunStart { ts } => {
                // 崩溃+重启：上一会话未 RunStop 即被本 RunStart 覆盖，
                // 先以该会话最后一个事件 ts 闭合，再开新段。
                if let (Some(a), Some(b)) = (open, last_event_after_open) {
                    if b > a {
                        spans.push((a, b));
                    }
                }
                open = Some(*ts);
                last_event_after_open = None;
            }
            StatsEvent::RunStop { ts } => {
                if let Some(a) = open.take() {
                    if *ts > a {
                        spans.push((a, *ts));
                    }
                }
                last_event_after_open = None;
            }
            _ => {
                if open.is_some() {
                    last_event_after_open = Some(ev.ts());
                }
            }
        }
    }
    if let (Some(a), Some(b)) = (open, last_event_after_open) {
        if b > a {
            spans.push((a, b));
        }
    }
    let mut out: BTreeMap<String, Vec<(i64, i64)>> = BTreeMap::new();
    for (a, b) in spans {
        let mut t = a;
        while t < b {
            let d0 = local_day_start(t, offset_minutes);
            let clip = (a.max(d0), b.min(d0 + 86_400));
            if clip.1 > clip.0 {
                out.entry(fmt_local_date(t, offset_minutes))
                    .or_default()
                    .push(clip);
            }
            let next = d0 + 86_400;
            if next <= t {
                break;
            }
            t = next;
        }
    }
    out
}

/// 中位数（偶数样本取中两值平均）。
fn median(mut xs: Vec<f64>) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = xs.len();
    if n % 2 == 1 {
        Some(xs[n / 2])
    } else {
        Some((xs[n / 2 - 1] + xs[n / 2]) / 2.0)
    }
}

/// 日均耗电双口径对账（融合稿冲突清单 1 落地）。
///
/// 上界 = 剔除粘滞标记日的每日放电实测斜率中位数 ×24h；下界 =
/// 窗口首尾总账（净降 + 充电回补）÷ 天数。两口径差异 >2 倍 →
/// conflict。呈现无条件区间化（lower/upper 成对给出，永不输出
/// 单值）；样本护栏降级为质量徽标（badges：样本不足/含粘滞）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct AvgEstimate {
    /// 区间下界（总账口径 %/天）。
    pub lower_pct_per_day: Option<f64>,
    /// 区间上界（中位数×24h 口径 %/天）。
    pub upper_pct_per_day: Option<f64>,
    /// 对账明细：剔除粘滞日后的斜率中位数（%/h）。
    pub median_slope_pct_per_hour: Option<f64>,
    /// 对账明细：总账值（%/天）。
    pub ledger_pct_per_day: Option<f64>,
    pub valid_days: usize,
    pub window_days: f64,
    pub sticky_days: usize,
    pub conflict: bool,
    pub badges: Vec<String>,
}

/// 双口径对账冲突判定（FR-3）：conflict = (u > R·l) || (u > 0 且
/// l > R·u)；u == 0 退化绝对阈值 l > 1.0 %/天；任一为负直接置真；
/// l <= 0 且 u > 0 分支语义保持（N-4 回归锚）。
fn avg_conflict(ledger: Option<f64>, upper: Option<f64>) -> bool {
    match (ledger, upper) {
        (Some(l), Some(u)) => {
            if l < 0.0 || u < 0.0 {
                true
            } else if u == 0.0 {
                l > AVG_CONFLICT_ABS_THRESHOLD_PCT
            } else if l <= 0.0 {
                u > 0.0
            } else {
                u > AVG_CONFLICT_RATIO * l || l > AVG_CONFLICT_RATIO * u
            }
        }
        _ => false,
    }
}

fn build_avg_estimate(
    events: &[StatsEvent],
    segments: &[Segment],
    device_key: &str,
    cards: &[DailyCard],
    sticky_days: &BTreeSet<String>,
) -> AvgEstimate {
    // 上界样本：非粘滞、非稀疏日的实测斜率。规格 §3③ 仅授权
    // 「剔除粘滞标记日」——零斜率的非粘滞有效日必须进入中位数样本
    // （复核批 F1：删去 *v > 0.0 过滤，该过滤把 0 斜率有效日错当
    // 粘滞日剔除）。FR-2 症状剔除：slope < 0 的日卡（充电净增益日
    // 负斜率合法产生）不进中位数样本（冲突 C-2 已裁：只剔 slope<0
    // 症状日，废弃 charging_spans / charge_recovery_pct 判据）。
    let slopes: Vec<f64> = cards
        .iter()
        .filter(|c| !c.sticky && !c.sparse)
        .filter_map(|c| c.slope_pct_per_hour)
        .filter(|v| *v >= 0.0)
        .collect();
    let valid_days = slopes.len();
    let median_slope = median(slopes);
    let upper = median_slope.map(|m| round2(m * 24.0));
    // 下界：窗口首尾总账（净降 + 充电回补）÷ 天数。
    let mut dev_reads: Vec<(i64, u8)> = events
        .iter()
        .filter_map(|ev| match ev {
            StatsEvent::Read(e)
                if e.physical_key == device_key
                    && !e.charging
                    && e.source != crate::stats::SOURCE_CHARGING_FAST =>
            {
                Some((e.ts, e.percent))
            }
            _ => None,
        })
        .collect();
    dev_reads.sort_by_key(|(ts, _)| *ts);
    let recovery: i16 = segments
        .iter()
        .filter(|s| s.device_key == device_key && s.seg_type == SegmentType::Charging)
        .map(|s| (s.end_pct as i16 - s.start_pct as i16).max(0))
        .sum();
    let (ledger, window_days) = match (dev_reads.first(), dev_reads.last()) {
        (Some((t0, p0)), Some((t1, p1))) if t1 > t0 => {
            let wd = (*t1 - *t0) as f64 / 86_400.0;
            let net = *p0 as i16 - *p1 as i16;
            (Some(round2((net + recovery) as f64 / wd)), round2(wd))
        }
        _ => (None, 0.0),
    };
    // FR-3（冲突 C-6 双向化 + u=0 护栏）：
    // conflict = (u > R·l) || (u > 0 且 l > R·u)——l 远超 u 的倒挂
    // 区间同样置真（渲染层两处无条件直出，无徽标会自相矛盾）；
    // u == 0 时比值式判据失效，退化为绝对阈值 l > 1.0 %/天；
    // l 或 u 任一为负直接置真；l <= 0 且 u > 0 的既有分支语义保持
    // （N-4 回归锚，含于比值式第一臂）。
    let conflict = avg_conflict(ledger, upper);
    let mut badges: Vec<String> = Vec::new();
    if valid_days < AVG_MIN_VALID_DAYS {
        badges.push("样本不足".to_string());
    }
    if !sticky_days.is_empty() {
        badges.push("含粘滞".to_string());
    }
    if conflict {
        badges.push("口径冲突".to_string());
    }
    AvgEstimate {
        lower_pct_per_day: ledger,
        upper_pct_per_day: upper,
        median_slope_pct_per_hour: median_slope,
        ledger_pct_per_day: ledger,
        valid_days,
        window_days,
        sticky_days: sticky_days
            .iter()
            .filter(|d| cards.iter().any(|c| &c.date == *d))
            .count(),
        conflict,
        badges,
    }
}

/// 周环比（融合稿 §3③/C-21：两个对比窗口均无粘滞标记且无口径冲突
/// 时才给箭头，否则「不可比」）。FR-5 两步走：窗口起止标注＋sparse
/// 剔除＋缺口徽标（不含日历日锚定，登记为后续可选优化——E 裁决）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct WeekOverWeek {
    pub suppressed: bool,
    pub reason: Option<String>,
    /// 近 7 天日均耗电 − 前 7 天日均耗电（%/天；suppressed 时 None；
    /// 日均分母剔除 sparse 日）。
    pub delta_pct_per_day: Option<f64>,
    /// 对比窗口首日（YYYY-MM-DD，最近 2×RECENT_DAYS 张日卡中最早
    /// 一张的日期；无日卡时 None）。
    pub window_first: Option<String>,
    /// 对比窗口末日（YYYY-MM-DD，最近一张日卡的日期；无日卡时 None）。
    pub window_last: Option<String>,
    /// 质量徽标（如「窗口含缺口」：日历缺口 >2 天不 suppress、
    /// 照常给 delta，冲突 C-4 已裁）。
    pub badges: Vec<String>,
}

/// `YYYY-MM-DD` → 自 epoch 起算的日序号（日差算术用；解析失败 None）。
fn civil_days(date: &str) -> Option<i64> {
    let mut it = date.split('-');
    let y: i64 = it.next()?.parse().ok()?;
    let m: i64 = it.next()?.parse().ok()?;
    let d: i64 = it.next()?.parse().ok()?;
    if it.next().is_some() || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Howard Hinnant days_from_civil（公历）。
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

fn build_week_over_week(cards: &[DailyCard], conflict: bool) -> WeekOverWeek {
    // 对比窗口 = 最近 2×RECENT_DAYS 张日卡（prev 7 + recent 7），
    // 窗口起止即窗口首末卡日期（FR-5 第一刀：纯展示标注）。
    let window: Vec<&DailyCard> = cards
        .iter()
        .rev()
        .take((2 * RECENT_DAYS) as usize)
        .collect();
    let window_first = window.last().map(|c| c.date.clone());
    let window_last = window.first().map(|c| c.date.clone());
    // 窗口内日历缺口检测：相邻两卡日期差 >2 天挂「窗口含缺口」
    // 徽标（不 suppress、照常给 delta；冲突 C-4 已裁）。
    let mut badges: Vec<String> = Vec::new();
    let mut has_gap = false;
    for pair in window.iter().rev().collect::<Vec<_>>().windows(2) {
        if let (Some(a), Some(b)) = (civil_days(&pair[0].date), civil_days(&pair[1].date)) {
            if b - a > WOW_GAP_THRESHOLD_DAYS {
                has_gap = true;
            }
        }
    }
    if has_gap {
        badges.push("窗口含缺口".to_string());
    }
    let recent: Vec<&DailyCard> = cards.iter().rev().take(RECENT_DAYS as usize).collect();
    let prev: Vec<&DailyCard> = cards
        .iter()
        .rev()
        .skip(RECENT_DAYS as usize)
        .take(RECENT_DAYS as usize)
        .collect();
    let sticky_hit = recent.iter().any(|c| c.sticky) || prev.iter().any(|c| c.sticky);
    if sticky_hit || conflict {
        return WeekOverWeek {
            suppressed: true,
            reason: Some("不可比".to_string()),
            delta_pct_per_day: None,
            window_first,
            window_last,
            badges,
        };
    }
    // FR-5 第二刀：sparse 日剔除出周均值（样本空则「样本不足」；
    // 20 分钟读数的 sparse 日对均值的系统性拉偏由此消除）。
    let recent_eff: Vec<&DailyCard> = recent.iter().copied().filter(|c| !c.sparse).collect();
    let prev_eff: Vec<&DailyCard> = prev.iter().copied().filter(|c| !c.sparse).collect();
    if prev_eff.is_empty() || recent_eff.is_empty() {
        return WeekOverWeek {
            suppressed: true,
            reason: Some("样本不足".to_string()),
            delta_pct_per_day: None,
            window_first,
            window_last,
            badges,
        };
    }
    let r: f64 =
        recent_eff.iter().map(|c| c.drain_pct as f64).sum::<f64>() / recent_eff.len() as f64;
    let p: f64 = prev_eff.iter().map(|c| c.drain_pct as f64).sum::<f64>() / prev_eff.len() as f64;
    WeekOverWeek {
        suppressed: false,
        reason: None,
        delta_pct_per_day: Some(round2(r - p)),
        window_first,
        window_last,
        badges,
    }
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

/// 渲染 CSV：BOM + manifest `#` 注释块（组5 自描述导出落点之一，
/// recheck.py 跳过 `#` 行后再字节对账）+ 表头 + 每段一行（行序 =
/// 展示键字典序 + start_ts；匿名化后 D1/D2… 顺序即哈希字典序）。
/// `generated_at` 为导出时刻 epoch 秒（进 manifest 注释块）。
pub fn render_csv(
    segments: &[Segment],
    anon: Option<&HashMap<String, String>>,
    offset_minutes: i32,
    generated_at: i64,
) -> String {
    let mut rows: Vec<(String, &Segment)> = segments
        .iter()
        .map(|s| (display_key(&s.device_key, anon), s))
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.start_ts.cmp(&b.1.start_ts)));
    let mut out = String::from("\u{feff}");
    out.push_str(&crate::export_manifest::manifest_csv_comments(generated_at));
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

/// 渲染 JSON 导出：`{version, exported_at, manifest, events:[...]}`——events 为
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
            StatsEvent::ActivityBuckets {
                ts,
                physical_key,
                days,
            } => serde_json::json!({
                "ts": ts,
                "event": "activity_buckets",
                "physical_key": display_key(physical_key, anon),
                "days": days,
            }),
        };
        arr.push(v);
    }
    serde_json::json!({
        "version": 1,
        "exported_at": exported_at,
        "manifest": crate::export_manifest::manifest_json(exported_at),
        "events": arr,
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// HTML 可视化报告（自包含单文件，内联 SVG/JS，零外部依赖）
// ---------------------------------------------------------------------------

/// volttray-data 机器可读数据块（AC-B8 复算对账唯一取数源）：分段明细
///（与 CSV 同数据同列）+ 图表①③底层数据 + 分日视图日卡 + 首屏 KPI +
/// 日均耗电双口径对账 + 周环比 + 灰态缺口卡。匿名化开启时仅含匿名化
/// 后键，不含 physical_key 明文（R5-I-04 并入）。顶层另含 manifest
/// 自描述块（组5 N-12②：字段字典＋生效脱敏规则 id 集＋schema 版本，
/// BvA-6 不含采集时区偏移值）。
pub fn build_report_data(
    events: &[StatsEvent],
    segments: &[Segment],
    anon: Option<&HashMap<String, String>>,
    offset_minutes: i32,
    generated_ts: i64,
) -> serde_json::Value {
    let anonymized = anon.is_some();
    // 设备清单（展示键 + 显示名；序 = 展示键字典序）；raw_of 为展示键
    // 到原始键的反映射——内部聚合（日卡 / 对账 / 粘滞）在原始键上做，
    // 透出前统一换展示键。
    let mut name_of: BTreeMap<String, String> = BTreeMap::new();
    let mut raw_of: HashMap<String, String> = HashMap::new();
    for ev in events {
        if let StatsEvent::Read(e) = ev {
            name_of.insert(display_key(&e.physical_key, anon), e.display_name.clone());
            raw_of.insert(display_key(&e.physical_key, anon), e.physical_key.clone());
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
    let (chart3_per_dev, chart3_total) = build_chart3(&segs);
    // 粘滞段 / 粘滞日（原始键口径，与 read 事件同键）。
    let sticky = sticky_spans(segments);
    let sticky_days = sticky_day_set(&sticky, offset_minutes);
    // 分日视图日卡（原始键）与 app 运行时段（全局口径，日界裁剪）。
    let daily_raw = build_daily_cards(events, segments, &sticky_days, offset_minutes);
    let run_spans_by_day = build_run_spans_by_day(events, offset_minutes);
    let now_ref = events.iter().map(|e| e.ts()).max().unwrap_or(generated_ts);
    let today = fmt_local_date(now_ref, offset_minutes);
    // 活动位图积累天数（跨事件按 date 去重并集；缺口卡判定）。
    // 复核批 F2：旧口径取「单事件 days.len() 最大值」——boundary/
    // run_stop 分次冲刷后同一日会散在多个事件里、单事件又可能携带
    // 多日，最大值既可能低估也可能高估；按 date 去重并集才是
    // 「已积累多少天」的真实口径。
    let mut activity_day_set: BTreeSet<&str> = BTreeSet::new();
    for ev in events {
        if let StatsEvent::ActivityBuckets { days, .. } = ev {
            for d in days {
                activity_day_set.insert(d.date.as_str());
            }
        }
    }
    let activity_days = activity_day_set.len();
    let days_covered = covered_days(segments, offset_minutes).len() as i64;
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
    // 逐设备透出：分日日卡 / 首屏 KPI / 双口径对账 / 周环比（键 = 展示键）。
    let mut daily_cards = serde_json::Map::new();
    let mut kpis = serde_json::Map::new();
    let mut avg_map = serde_json::Map::new();
    let mut wow_map = serde_json::Map::new();
    for (dkey, _name) in &devices {
        let raw_key = raw_of.get(dkey).cloned().unwrap_or_else(|| dkey.clone());
        let cards = daily_raw.get(&raw_key).cloned().unwrap_or_default();
        let dev_sticky_days = sticky_days.get(&raw_key).cloned().unwrap_or_default();
        let avg = build_avg_estimate(events, segments, &raw_key, &cards, &dev_sticky_days);
        let wow = build_week_over_week(&cards, avg.conflict);
        let latest = events.iter().rev().find_map(|ev| match ev {
            StatsEvent::Read(e) if e.physical_key == raw_key => Some((e.ts, e.percent, e.charging)),
            _ => None,
        });
        let today_card = cards.iter().find(|c| c.date == today);
        let today_run_secs: i64 = run_spans_by_day
            .get(&today)
            .map(|v| v.iter().map(|(a, b)| b - a).sum())
            .unwrap_or(0);
        // 「今日已耗」与日卡同一护栏：样本不足时降级灰态、只给首末差
        // （drain 置 None，由展示层走 emptyMark 灰态语言）。
        let (today_drain, today_sparse, today_sticky) = match today_card {
            Some(c) => (
                if c.sparse { None } else { Some(c.drain_pct) },
                c.sparse,
                c.sticky,
            ),
            None => (None, true, false),
        };
        daily_cards.insert(
            dkey.clone(),
            serde_json::to_value(&cards).unwrap_or_default(),
        );
        avg_map.insert(dkey.clone(), serde_json::to_value(&avg).unwrap_or_default());
        wow_map.insert(dkey.clone(), serde_json::to_value(&wow).unwrap_or_default());
        kpis.insert(
            dkey.clone(),
            serde_json::json!({
                "current_pct": latest.map(|(_, p, _)| p),
                "current_ts": latest.map(|(t, _, _)| t),
                "current_charging": latest.map(|(_, _, c)| c).unwrap_or(false),
                "today_date": today,
                "today_drain_pct": today_drain,
                "today_sparse": today_sparse,
                "today_sticky": today_sticky,
                "today_run_secs": today_run_secs,
                // 推断项无条件区间化：只有 lower/upper 成对字段，不存在
                // 任何单值字段（首屏准入按护栏完备性，非按实测/推断切）。
                "avg": {
                    "lower_pct_per_day": avg.lower_pct_per_day,
                    "upper_pct_per_day": avg.upper_pct_per_day,
                    "conflict": avg.conflict,
                    "badges": avg.badges,
                },
            }),
        );
    }
    // 图表①序列补粘滞段标注源（透出键 = 展示键）。
    let mut chart1_json = serde_json::to_value(&chart1).unwrap_or_default();
    if let Some(arr) = chart1_json.as_array_mut() {
        for s in arr.iter_mut() {
            let dk = s["device_key"].as_str().unwrap_or("").to_string();
            let raw_key = raw_of.get(&dk).cloned().unwrap_or_else(|| dk.clone());
            let spans = sticky.get(&raw_key).cloned().unwrap_or_default();
            s["sticky_spans"] = serde_json::json!(spans);
        }
    }
    // 灰态缺口卡（指标④：无字段不伪造；位图积累 ≥7 天后 Phase 2 升级
    // 分日热力带，当前版本一律按缺口口径展示）。
    let gap_present = activity_days < AVG_MIN_VALID_DAYS;
    let gap_text = if activity_days == 0 {
        "尚无静止/活动字段，不用 app 协议流量伪造。补齐计划：activity_buckets 分钟位图随系统事件流积累（零设备查询、事件驱动），积累 ≥7 天后升级为分日热力带。"
            .to_string()
    } else if gap_present {
        format!(
            "静止/活动分钟位图已积累 {activity_days} 天（≥7 天升级分日热力带）；当前版本仍按缺口口径展示，不用 app 协议流量伪造。"
        )
    } else {
        format!(
            "静止/活动分钟位图已积累 {activity_days} 天，满足升级条件；分日热力带将在后续版本上线，当前仍按缺口口径展示，不用 app 协议流量伪造。"
        )
    };
    serde_json::json!({
        "meta": {
            "version": 2,
            "generated_at": generated_ts,
            "generated_at_local": fmt_rfc3339(generated_ts, offset_minutes),
            "tz_offset": tz,
            "anonymized": anonymized,
            "days_covered": days_covered,
            "sparse_note": sparse_note,
            "chart1_bucket_secs": CHART1_BUCKET_SECS,
            "chart1_default_window_days": CHART1_DEFAULT_WINDOW_DAYS,
            "daily_view_day_limit": DAILY_VIEW_DAY_LIMIT,
            "activity_days": activity_days,
            "device_count": devices.len(),
            "segment_count": segs.len(),
        },
        "manifest": crate::export_manifest::manifest_json(generated_ts),
        "devices": devices,
        "segments": seg_rows,
        "chart1": chart1_json,
        "chart3": { "per_device": chart3_per_dev, "total": chart3_total },
        "daily_cards": daily_cards,
        "run_spans_by_day": run_spans_by_day,
        "first_screen_kpis": kpis,
        "avg_estimate": avg_map,
        "week_over_week": wow_map,
        "activity_gap_note": { "present": gap_present, "text": gap_text },
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
h1{font-size:20px;margin:0 0 4px}h2{font-size:16px;margin:24px 0 8px}h3{font-size:14px;margin:16px 0 6px}
.meta{color:#666;font-size:13px}.sparse{color:#b26a00;font-size:13px}
.cards{display:flex;flex-wrap:wrap;gap:12px;margin:12px 0}
.card{border:1px solid #ccc5;border-radius:8px;padding:10px 14px;min-width:170px}
.card .k{font-size:12px;color:#777}.card .v{font-size:18px;font-weight:600}
.card .s{font-size:12px;color:#777;margin-top:2px}
.card.dim{opacity:.65;border-style:dashed}
.badge{display:inline-block;font-size:11px;border-radius:4px;padding:0 6px;margin:4px 4px 0 0;border:1px solid #9995}
.b-measure{color:#2f855a;border-color:#2f855a55}
.b-infer{color:#b7791f;border-color:#b7791f55}
.b-gap{color:#888;border-color:#8888}
.b-warn{color:#b26a00;border-color:#b26a0055}
.b-conflict{color:#c53030;border-color:#c5303055}
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
.dayrow{display:flex;align-items:center;gap:8px;margin:2px 0;font-size:12px;flex-wrap:wrap}
.dayrow .dl{width:64px;color:#777;flex:none}
.dayrow .dd{color:#777;min-width:230px}
footer{margin-top:28px;border-top:1px solid #ccc5;padding-top:10px;font-size:12.5px;color:#777}
footer table{font-size:12px}
</style>
</head>
<body>
<header>
<h1>LogiVoltTray 耗电统计报告</h1>
<p class="meta">__TITLE_BITS__</p>
<p class="sparse">__SPARSE_NOTE__</p>
</header>

<h2>首屏答案区</h2>
<section class="cards" id="kpis"></section>

<h2>分日视图（日界对齐：日耗电柱 + 24h 时间轴）</h2>
<p class="hint">日耗电 = 当日放电快照首末差（跨日段在日界处切分、不摊入；charging-fast 事件还原段剔除）。时间轴实色段为 app 运行时段（监控覆盖，app 进程口径），绿色为充电段，灰罩为未知段。质量标签：设备上报粘滞（当日斜率可能低估）/ 样本不足（read&lt;3 或覆盖&lt;2h，只给首末差）。</p>
<div id="daily"></div>

<h2>静止 vs 活动</h2>
<div id="gap"></div>

<h2>趋势佐证区</h2>
<h3>电量曲线（按小时聚合取末值）</h3>
<div class="controls" id="c1ctl">
<button data-win="90" class="on">近 90 天</button><button data-win="30">近 30 天</button><button data-win="0">全部历史</button>
</div>
<div class="scroll"><svg id="c1" width="960" height="280" role="img" aria-label="电量曲线"></svg></div>
<p class="hint">跨未知段断线并以灰罩标示（不直线相连）；充电段以底色标记；粘滞段以黄褐色罩层与虚线标注（悬停看说明）；窗口可切换，默认近 90 天。</p>
<h3>日均耗电双口径对账明细</h3>
<div class="cards" id="recon"></div>
<p class="hint">上界 = 剔除粘滞标记日的每日放电实测斜率中位数 ×24h（系统性偏高：非运行时段按运行斜率外推）；下界 = 窗口首尾总账（净降 + 充电回补）÷ 天数（按全天摊薄，可能偏低）。差异 &gt;2 倍挂「口径冲突」。物理量（mAh/mWh）无电池容量字段，不给。</p>

<h2>证据与口径区</h2>
<h3>app 运行时长占比</h3>
<div class="scroll"><svg id="c3" width="960" height="180" role="img" aria-label="时长占比"></svg></div>
<div class="legend" id="c3legend"></div>
<h3>分段明细（与 CSV 同数据同列）</h3>
<div class="tblwrap"><table id="seg"><thead><tr>
<th>device_key</th><th>display_name</th><th>segment_type</th><th>start_ts</th><th>end_ts</th><th>duration_min</th><th>start_pct</th><th>end_pct</th><th>delta_pct</th><th>rate_pct_per_hour</th><th>note</th>
</tr></thead><tbody></tbody></table></div>

<footer>
<h3>口径与边界说明</h3>
<table id="glossary"><thead><tr><th>展示项</th><th>形态</th><th>数据来源字段</th><th>口径</th><th>护栏</th></tr></thead><tbody>
<tr><td>当前电量</td><td>首屏 KPI 卡</td><td>percent/ts 最新快照</td><td>实测</td><td>—</td></tr>
<tr><td>今日已耗</td><td>首屏 KPI 卡</td><td>当日 discharging 首末差</td><td>实测</td><td>read&lt;3 或覆盖&lt;2h → 灰态「数据不足」</td></tr>
<tr><td>今日 app 运行时长</td><td>首屏 KPI 卡</td><td>run_start/stop 当日段</td><td>实测（app 进程口径）</td><td>文案守卫</td></tr>
<tr><td>日均耗电区间</td><td>首屏 KPI 区间卡</td><td>剔除粘滞日中位数 × 总账对账</td><td>推断</td><td>无条件区间化 + 差异&gt;2 倍冲突提示 + 样本质量徽标</td></tr>
<tr><td>每日耗电量</td><td>分日柱 + 明细</td><td>percent/charging/ts</td><td>实测</td><td>粘滞 / 样本不足两级标签</td></tr>
<tr><td>app 运行时段</td><td>24h 分日时间轴</td><td>run_start/run_stop</td><td>实测（改名防误导）</td><td>口径守卫测试</td></tr>
<tr><td>充电时段</td><td>时间轴绿色段</td><td>charging-fast 事件</td><td>实测</td><td>—</td></tr>
<tr><td>静止 vs 活动</td><td>灰态卡→热力带</td><td>activity_buckets（新增）</td><td>缺口→实测</td><td>禁用流量代理</td></tr>
<tr><td>电量曲线</td><td>折线 + 平台期标注</td><td>percent/ts 全量快照</td><td>实测</td><td>粘滞段标注 + tooltip</td></tr>
<tr><td>日均耗电对账明细</td><td>P1 明细卡</td><td>双口径全量字段</td><td>推断</td><td>冲突状态明示</td></tr>
<tr><td>app 运行时长占比</td><td>堆叠横条</td><td>分段时长合计</td><td>实测</td><td>改名</td></tr>
<tr><td>分段明细表</td><td>表格（证据层）</td><td>与 CSV 同数据同列</td><td>实测</td><td>rate 护栏（duration&lt;5min 或 delta 绝对值&lt;1% → None）</td></tr>
</tbody></table>
<p>工程口径：设备数 <b id="mdev">—</b>、聚合段数 <b id="mseg">—</b>、覆盖天数 <b id="mday">—</b>（不占首屏）。日界 = 本地 00:00（时区见页首）。日界按单一 UTC 偏移计算，跨夏令时切换日前后历史日存在 ≤1h 归属偏差（不做事件级时区库重算）。</p>
<p>已移除：跨日段线性摊入柱图（日耗电口径已统一为当日首末差，避免同页双口径）；近 7 天耗速率摘要卡（分母语义错误：app 运行段由 run_start/run_stop 事件锚定）。</p>
<p>单文件离线报告：零外部依赖，断网可开。</p>
</footer>

<script id="volttray-data" type="application/json">__VOLTTRAY_DATA__</script>
<script>
"use strict";
var D=JSON.parse(document.getElementById("volttray-data").textContent);
var PALETTE=["#2b6cb0","#c53030","#2f855a","#b7791f","#6b46c1","#c05621","#2c7a7b","#97266d"];
var TYPE_COLOR={"active":"#2b6cb0","sleep":"#6b46c1","charging":"#2f855a","unknown":"#a0aec0"};
var TYPE_LABEL={"active":"app 运行时段（监控覆盖）","sleep":"休眠","charging":"充电","unknown":"未知"};
function esc(s){return String(s).replace(/&/g,"&amp;").replace(/</g,"&lt;").replace(/>/g,"&gt;");}
function el(tag,attrs,parent){var e=document.createElementNS("http://www.w3.org/2000/svg",tag);for(var k in attrs)e.setAttribute(k,attrs[k]);if(parent)parent.appendChild(e);return e;}
function offSecs(){var m=D.meta.tz_offset;var sign=m[0]==="-"?-1:1;var p=m.slice(1).split(":");return ((+p[0])*60+(+p[1]))*sign*60;}
function fmtTs(ts){var d=new Date((ts+offSecs())*1000);function z(n){return (n<10?"0":"")+n;}return d.getUTCFullYear()+"-"+z(d.getUTCMonth()+1)+"-"+z(d.getUTCDate())+" "+z(d.getUTCHours())+":"+z(d.getUTCMinutes());}
function fmtDur(sec){sec=Math.max(0,Math.round(sec));var h=Math.floor(sec/3600),m=Math.round((sec%3600)/60);return h>0?h+" 小时 "+m+" 分":m+" 分";}
// G-28：读数年龄（展示层，基准=meta.generated_at 导出时刻；超 2h 醒目）。
function ageText(ts){var s=Math.max(0,(D.meta.generated_at||0)-ts);var m=Math.floor(s/60);if(m<1)return "刚刚";if(m<60)return m+" 分钟前";return Math.floor(m/60)+" 小时前";}
function ageStale(ts){return ts&&(D.meta.generated_at-ts)>7200;}
function emptyMark(id){var n=document.getElementById(id);if(n)n.outerHTML='<div class="empty">暂无数据</div>';}
function badge(t,cls){return '<span class="badge '+cls+'">'+esc(t)+'</span>';}
var segs=D.segments||[];
var devs=D.devices||[];
function devName(k){for(var i=0;i<devs.length;i++)if(devs[i][0]===k)return devs[i][1];return k;}

// 首屏答案区：KPI 卡带四卡（当前电量 / 今日已耗 / 今日 app 运行时长 / 日均耗电区间）
(function(){
  var host=document.getElementById("kpis");
  if(!devs.length){host.innerHTML='<div class="empty">暂无数据</div>';return;}
  var html="";
  devs.forEach(function(dv){
    var k=dv[0],name=dv[1];var kpi=(D.first_screen_kpis||{})[k]||{};
    html+='<div class="card"><div class="k">当前电量 · '+esc(name)+'</div><div class="v">'
      +(kpi.current_pct==null?"—":kpi.current_pct+"%")+'</div><div class="s">'
      +(kpi.current_ts?fmtTs(kpi.current_ts)+" · 更新于 "+ageText(kpi.current_ts):"")+(kpi.current_charging?" · 充电中":"")+'</div>'
      +badge("实测","b-measure")+(ageStale(kpi.current_ts)?badge("读数超 2h 未刷新","b-warn"):"")+'</div>';
    if(kpi.today_sparse||kpi.today_drain_pct==null){
      html+='<div class="card dim"><div class="k">今日已耗 · '+esc(name)+'</div><div class="v">数据不足</div>'
        +'<div class="s">read&lt;3 或覆盖&lt;2h 时不给斜率、只给首末差</div>'
        +badge("实测","b-measure")+badge("样本不足","b-warn")+'</div>';
    }else{
      html+='<div class="card"><div class="k">今日已耗 · '+esc(name)+'</div><div class="v">'+kpi.today_drain_pct+'%</div>'
        +'<div class="s">'+(kpi.today_date||"")+' 首末差</div>'
        +badge("实测","b-measure")+(kpi.today_sticky?badge("设备上报粘滞","b-warn"):"")+'</div>';
    }
    html+='<div class="card"><div class="k">今日 app 运行时长 · '+esc(name)+'</div><div class="v">'
      +fmtDur(kpi.today_run_secs||0)+'</div><div class="s">app 进程口径（监控覆盖）</div>'
      +badge("实测","b-measure")+'</div>';
    var avg=kpi.avg||{};
    if(avg.lower_pct_per_day==null&&avg.upper_pct_per_day==null){
      html+='<div class="card dim"><div class="k">日均耗电区间 · '+esc(name)+'</div><div class="v">数据不足</div>'
        +'<div class="s">无有效放电样本</div>'+badge("推断","b-infer")+'</div>';
    }else{
      var L=avg.lower_pct_per_day==null?"—":avg.lower_pct_per_day;
      var U=avg.upper_pct_per_day==null?"—":avg.upper_pct_per_day;
      var bds=badge("推断","b-infer");
      (avg.badges||[]).forEach(function(b){bds+=badge(b,b==="口径冲突"?"b-conflict":"b-warn");});
      html+='<div class="card"><div class="k">日均耗电区间 · '+esc(name)+'</div><div class="v">'
        +L+" ~ "+U+' %/天</div><div class="s">剔除粘滞日中位数 × 总账对账</div>'+bds+'</div>';
    }
  });
  host.innerHTML=html;
})();

// 分日视图：日耗电柱 + 24h 时间轴（同一日界）；窗口 >daily_view_day_limit 天降级周聚合
(function(){
  var host=document.getElementById("daily");
  var cardsMap=D.daily_cards||{};var runByDay=D.run_spans_by_day||{};
  var limit=D.meta.daily_view_day_limit||90;var any=false;var html="";var jobs=[];
  devs.forEach(function(dv,di){
    var k=dv[0],name=dv[1];var cards=(cardsMap[k]||[]);if(!cards.length)return;any=true;
    var weekly=cards.length>limit;
    html+='<h3>'+esc(name)+'（'+esc(k)+'）</h3>';
    if(weekly)html+='<p class="hint">窗口超过 '+limit+' 天：日卡降级为周聚合，时间轴抽稀（阈值写死于代码常量）。</p>';
    var bars=[];
    if(weekly){
      var wk={},order=[];
      cards.forEach(function(c){
        var d=Date.parse(c.date+"T00:00:00Z")/86400000;var w=Math.floor((d+3)/7);
        if(!wk[w]){wk[w]={drain:0,label:c.date.slice(5)+" 起周",sticky:false};order.push(w);}
        wk[w].drain+=c.drain_pct;wk[w].sticky=wk[w].sticky||c.sticky;
      });
      order.sort(function(a,b){return a-b;});
      bars=order.map(function(w){return wk[w];});
    }else{
      bars=cards.map(function(c){return {drain:c.drain_pct,label:c.date.slice(5),sticky:c.sticky,sparse:c.sparse};});
    }
    var sid="db"+di;
    html+='<div class="scroll"><svg id="'+sid+'" width="960" height="150" role="img" aria-label="每日耗电柱"></svg></div>';
    jobs.push({type:"bars",id:sid,bars:bars});
    if(!weekly){
      cards.forEach(function(c,ci){
        var runs=runByDay[c.date]||[];var rsec=0;
        runs.forEach(function(r){rsec+=Math.max(0,r[1]-r[0]);});
        var tags="";
        if(c.sticky)tags+=badge("设备上报粘滞","b-warn");
        if(c.sparse)tags+=badge("样本不足","b-warn");
        var det="耗电 "+c.drain_pct+"%（"+c.first_pct+"→"+c.last_pct+"） · app 运行 "+fmtDur(rsec)
          +" · 充电回补 +"+c.charge_recovery_pct+"%";
        if(c.slope_pct_per_hour!=null)det+=" · 斜率 "+c.slope_pct_per_hour+"%/h";
        var tid="tl"+di+"_"+ci;
        html+='<div class="dayrow"><span class="dl">'+c.date.slice(5)+'</span>'
          +'<svg id="'+tid+'" width="480" height="14" role="img" aria-label="当日时间轴"></svg>'
          +'<span class="dd">'+esc(det)+'</span><span>'+tags+'</span></div>';
        jobs.push({type:"tl",id:tid,card:c,runs:runs});
      });
    }
  });
  if(!any){host.innerHTML='<div class="empty">暂无数据</div>';return;}
  host.innerHTML=html;
  jobs.forEach(function(j){
    var svg=document.getElementById(j.id);if(!svg)return;
    if(j.type==="bars"){
      var W=960,H=150,PL=30,PB=22,PT=8;var maxV=0.5;
      j.bars.forEach(function(b){if(b.drain>maxV)maxV=b.drain;});
      var n=j.bars.length;var bw=Math.max(6,Math.min(40,(W-PL-10)/n-4));
      j.bars.forEach(function(b,i){
        var x=PL+((W-PL-10)/n)*i+2;var h=b.drain/maxV*(H-PT-PB);
        var r=el("rect",{x:x,y:H-PB-h,width:bw,height:Math.max(0,h),fill:"#2b6cb0",
          "fill-opacity":b.sparse?0.35:(b.sticky?0.55:0.9)},svg);
        var t=el("title",{},r);
        t.textContent=b.sticky?"设备上报粘滞：当日斜率可能低估":(b.sparse?"样本不足：只给首末差":b.label+" 耗电 "+b.drain+"%");
        if(n<=62||i%Math.ceil(n/62)===0){
          var tx=el("text",{x:x+bw/2-12,y:H-PB+12,"font-size":9,fill:"#888"},svg);tx.textContent=b.label;
        }
      });
      el("line",{x1:PL,y1:H-PB,x2:W-8,y2:H-PB,stroke:"#888","stroke-width":0.5},svg);
    }else{
      var c=j.card;var d0=Date.parse(c.date+"T00:00:00Z")/1000-offSecs();
      function X(t){return (t-d0)/86400*480;}
      function spans(list,color,op){
        (list||[]).forEach(function(s){
          var x0=Math.max(0,X(s[0])),x1=Math.min(480,X(s[1]));
          if(x1>x0)el("rect",{x:x0,y:0,width:x1-x0,height:14,fill:color,"fill-opacity":op},svg);
        });
      }
      spans(c.unknown_spans,"#a0aec0",0.5);
      spans(c.charging_spans,"#2f855a",0.85);
      spans(j.runs,"#2b6cb0",0.85);
    }
  });
})();

// 静止 vs 活动：灰态缺口卡（无字段不伪造；位图积累 ≥7 天后升级分日热力带）
(function(){
  var host=document.getElementById("gap");
  var g=D.activity_gap_note||{present:true,text:""};
  host.innerHTML='<div class="card dim" style="max-width:720px"><div class="k">静止 vs 活动</div>'
    +'<div class="v" style="font-size:14px;font-weight:400">'+esc(g.text||"")+'</div>'
    +badge(g.present?"缺口":"待升级","b-gap")+'</div>';
})();

// 电量曲线：未知段断线 + 灰罩；充电底色；粘滞段黄褐罩层 + tooltip；单 polyline，禁逐点元素
(function(){
  if(!D.chart1||!D.chart1.length){emptyMark("c1");return;}
  var svg=document.getElementById("c1");var W=960,H=280,PL=36,PR=8,PT=10,PB=26;
  var maxTs=0,minTs=Infinity,i,j;
  for(i=0;i<segs.length;i++){if(segs[i].end_ts>maxTs)maxTs=segs[i].end_ts;if(segs[i].start_ts<minTs)minTs=segs[i].start_ts;}
  if(!isFinite(minTs)){emptyMark("c1");return;}
  function draw(winDays){
    while(svg.firstChild)svg.removeChild(svg.firstChild);
    var lo=winDays>0?Math.max(minTs,maxTs-winDays*86400):minTs,hi=maxTs;
    if(hi<=lo)hi=lo+3600;
    var hours=(hi-lo)/3600;
    var w=Math.max(W,Math.min(9600,PL+PR+Math.ceil(hours*2)));
    svg.setAttribute("width",w);svg.setAttribute("height",H);
    function X(t){return PL+(t-lo)/(hi-lo)*(w-PL-PR);}
    function Y(p){return PT+(100-p)/100*(H-PT-PB);}
    for(i=0;i<D.chart1.length;i++){var sr=D.chart1[i];
      for(j=0;j<sr.unknown_spans.length;j++){var u=sr.unknown_spans[j];if(u[1]<lo||u[0]>hi)continue;
        el("rect",{x:X(Math.max(u[0],lo)),y:PT,width:Math.max(1,X(Math.min(u[1],hi))-X(Math.max(u[0],lo))),height:H-PT-PB,fill:"#a0aec0","fill-opacity":0.18},svg);}
      for(j=0;j<sr.charging_spans.length;j++){var c=sr.charging_spans[j];if(c[1]<lo||c[0]>hi)continue;
        el("rect",{x:X(Math.max(c[0],lo)),y:PT,width:Math.max(1,X(Math.min(c[1],hi))-X(Math.max(c[0],lo))),height:H-PT-PB,fill:"#2f855a","fill-opacity":0.10},svg);}
      for(j=0;j<(sr.sticky_spans||[]).length;j++){var st=sr.sticky_spans[j];if(st[1]<lo||st[0]>hi)continue;
        var rx=X(Math.max(st[0],lo)),rw=Math.max(1,X(Math.min(st[1],hi))-rx);
        var rc=el("rect",{x:rx,y:PT,width:rw,height:H-PT-PB,fill:"#b7791f","fill-opacity":0.10},svg);
        var tt=el("title",{},rc);tt.textContent="设备上报粘滞段：读数长期不变，斜率可能低估";
        el("line",{x1:rx,y1:PT+2,x2:rx+rw,y2:PT+2,stroke:"#b7791f","stroke-width":1.5,"stroke-dasharray":"4 3"},svg);}}
    el("line",{x1:PL,y1:Y(0),x2:w-PR,y2:Y(0),stroke:"#888","stroke-width":0.5},svg);
    el("line",{x1:PL,y1:PT,x2:PL,y2:H-PB,stroke:"#888","stroke-width":0.5},svg);
    [0,25,50,75,100].forEach(function(p){var t=el("text",{x:4,y:Y(p)+4,"font-size":10,fill:"#888"},svg);t.textContent=p;});
    var tickN=Math.min(12,Math.max(2,Math.floor(hours/24)+1));
    for(i=0;i<tickN;i++){var tt2=lo+(hi-lo)*i/(tickN-1);var tx=el("text",{x:X(tt2)-26,y:H-8,"font-size":10,fill:"#888"},svg);tx.textContent=fmtTs(tt2).slice(0,10);}
    for(i=0;i<D.chart1.length;i++){var sr2=D.chart1[i];var col=PALETTE[i%PALETTE.length];var run=[];
      var flush=function(){if(run.length>1){el("polyline",{points:run.join(" "),fill:"none",stroke:col,"stroke-width":1.4},svg);}run=[];};
      for(j=0;j<sr2.points.length;j++){var pt=sr2.points[j];if(pt[0]<lo-3599||pt[0]>hi)continue;
        if(pt[1]==null){flush();}else{run.push(X(pt[0]+1800).toFixed(1)+","+Y(pt[1]).toFixed(1));}}
      flush();}
    for(i=0;i<D.chart1.length;i++){var lx=el("text",{x:PL+8+i*150,y:PT+12,"font-size":11,fill:PALETTE[i%PALETTE.length]},svg);lx.textContent=D.chart1[i].device_key+" "+D.chart1[i].display_name;}
  }
  draw(90);
  var btns=document.querySelectorAll("#c1ctl button");
  btns.forEach(function(b){b.addEventListener("click",function(){
    btns.forEach(function(x){x.className="";});b.className="on";draw(+b.getAttribute("data-win"));});});
})();

// 日均耗电双口径对账明细卡（无条件区间化 + 冲突状态明示 + 周环比抑制）
(function(){
  var host=document.getElementById("recon");var m=D.avg_estimate||{};var w=D.week_over_week||{};
  var keys=Object.keys(m);if(!keys.length){host.innerHTML='<div class="empty">暂无数据</div>';return;}
  var html="";
  keys.forEach(function(k){
    var a=m[k];var name=devName(k);
    function f(v,unit){return v==null?"—":v+unit;}
    var bds="";
    (a.badges||[]).forEach(function(b){bds+=badge(b,b==="口径冲突"?"b-conflict":"b-warn");});
    var wow=w[k]||{suppressed:true};
    var wbadges="";(wow.badges||[]).forEach(function(b){wbadges+=badge(b,"b-warn");});
    var wwin=(wow.window_first&&wow.window_last)?(' · 窗口 '+wow.window_first+' ~ '+wow.window_last):"";
    var wtxt=wow.suppressed?badge(wow.reason||"不可比","b-warn")
      :("周环比 "+(wow.delta_pct_per_day>0?"↑ +":"↓ ")+wow.delta_pct_per_day+" %/天");
    html+='<div class="card"><div class="k">日均耗电对账 · '+esc(name)+'</div>'
      +'<div class="v">'+f(a.lower_pct_per_day,"")+' ~ '+f(a.upper_pct_per_day,"")+' %/天</div>'
      +'<div class="s">总账（下界）'+f(a.ledger_pct_per_day," %/天")
      +' · 中位斜率 '+f(a.median_slope_pct_per_hour," %/h")+'（×24h = 上界）<br>'
      +'有效样本 '+a.valid_days+' 天 · 窗口 '+a.window_days+' 天 · 粘滞 '+a.sticky_days+' 天'
      +' · 口径冲突：'+(a.conflict?"是":"否")+'<br>'+wtxt+wwin+'</div>'
      +badge("推断","b-infer")+bds+wbadges+'</div>';
  });
  host.innerHTML=html;
})();

// app 运行时长占比（每设备一条堆叠横条 + 合计）
(function(){
  if(!D.chart3||!D.chart3.total){emptyMark("c3");return;}
  var svg=document.getElementById("c3");var W=960,PL=110,PT=14,RH=22,GAP=8;
  var rows=[];var pd=D.chart3.per_device||{};
  Object.keys(pd).forEach(function(k){rows.push([k,pd[k]]);});
  rows.push(["合计",D.chart3.total]);
  if(rows.length<=1&&D.chart3.total.active+D.chart3.total.sleep+D.chart3.total.charging+D.chart3.total.unknown<=0){emptyMark("c3");document.getElementById("c3legend").innerHTML="";return;}
  var H=PT+rows.length*(RH+GAP)+8;svg.setAttribute("height",H);
  rows.forEach(function(r,i){
    var k=r[0],v=r[1];var tot=(v.active||0)+(v.sleep||0)+(v.charging||0)+(v.unknown||0);
    var y=PT+i*(RH+GAP);var t=el("text",{x:4,y:y+RH-7,"font-size":11,fill:"#888"},svg);t.textContent=k;
    if(tot<=0){var e=el("text",{x:PL,y:y+RH-7,"font-size":11,fill:"#aaa"},svg);e.textContent="暂无数据";return;}
    var x=PL;
    ["active","sleep","charging","unknown"].forEach(function(tp){
      var w=(v[tp]||0)/tot*(W-PL-90);
      if(w>0){el("rect",{x:x,y:y,width:w,height:RH,fill:TYPE_COLOR[tp],"fill-opacity":tp==="unknown"?0.55:0.85},svg);x+=w;}});
    var pct=el("text",{x:x+6,y:y+RH-7,"font-size":10,fill:"#888"},svg);
    pct.textContent=Math.round((v.active||0)/tot*100)+"/"+Math.round((v.sleep||0)/tot*100)+"/"+Math.round((v.charging||0)/tot*100)+"/"+Math.round((v.unknown||0)/tot*100)+"%";
  });
  document.getElementById("c3legend").innerHTML='<span><i style="background:#2b6cb0"></i>app 运行时段（监控覆盖）</span>'
    +'<span><i style="background:#6b46c1"></i>休眠</span><span><i style="background:#2f855a"></i>充电</span>'
    +'<span><i style="background:#a0aec0"></i>未知</span>';
})();

// 分段明细表（与 CSV 同数据同列；segment_type 保留原始枚举值）
(function(){
  var tb=document.querySelector("#seg tbody");
  if(!segs.length){tb.innerHTML='<tr><td colspan="11" style="text-align:center;color:#888">暂无数据</td></tr>';return;}
  var rows="";
  for(var i=0;i<segs.length;i++){var s=segs[i];
    rows+="<tr><td>"+esc(s.device_key)+"</td><td>"+esc(s.display_name)+"</td><td>"+s.segment_type+"</td><td>"
      +fmtTs(s.start_ts)+"</td><td>"+fmtTs(s.end_ts)+"</td><td>"+s.duration_min+"</td><td>"+s.start_pct+"</td><td>"
      +s.end_pct+"</td><td>"+s.delta_pct+"</td><td>"+(s.rate_pct_per_hour==null?"":s.rate_pct_per_hour.toFixed(2))
      +"</td><td>"+esc(s.note)+"</td></tr>";}
  tb.innerHTML=rows;
})();

// 页尾工程口径（不占首屏）
document.getElementById("mdev").textContent=D.meta.device_count;
document.getElementById("mseg").textContent=D.meta.segment_count;
document.getElementById("mday").textContent=D.meta.days_covered;
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

    pub(super) const CST: i32 = 480; // +08:00

    pub(super) fn ev_read(ts: i64, key: &str, pct: u8, charging: bool, source: &str) -> StatsEvent {
        StatsEvent::Read(ReadEvent {
            ts,
            physical_key: key.to_string(),
            display_name: format!("Name-{key}"),
            percent: pct,
            charging,
            source: source.to_string(),
        })
    }

    pub(super) fn ev_poll(ts: i64, key: &str, pct: u8) -> StatsEvent {
        ev_read(ts, key, pct, false, SOURCE_POLL)
    }

    pub(super) fn seg(
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

    /// 小修批第 4 项：时长 < 60s 的短段速率失真（v0.3.23 真机末行
    /// 9s 段算出 -400%/h），护栏统一返回 None；≥60s 不受影响。
    #[test]
    fn rate_pct_per_hour_guard_below_min_duration() {
        // 真机复现形：71→70、9s 段。
        let s9 = seg("d1", SegmentType::Active, 1000, 1009, 71, 70, &[]);
        assert_eq!(s9.rate_pct_per_hour(), None);
        // 边界：299s 拦、300s 放（-1%/300s = -12%/h；融合稿 rate 护栏：
        // duration<5min 或 |Δ%|<1 → None）。
        let s299 = seg("d1", SegmentType::Active, 1000, 1299, 71, 70, &[]);
        assert_eq!(s299.rate_pct_per_hour(), None);
        let s300 = seg("d1", SegmentType::Active, 1000, 1300, 71, 70, &[]);
        assert_eq!(s300.rate_pct_per_hour(), Some(-12.0));
        // Δ%=0 的段不论时长都不出速率（钉 86% 平台期 0%/h 伪值）。
        let s0d = seg("d1", SegmentType::Active, 1000, 4600, 70, 70, &[]);
        assert_eq!(s0d.rate_pct_per_hour(), None);
        // 护栏同样覆盖 charging 段；unknown 段仍恒 None。
        let c30 = seg("d1", SegmentType::Charging, 1000, 1030, 70, 72, &[]);
        assert_eq!(c30.rate_pct_per_hour(), None);
        let u1h = seg("d1", SegmentType::Unknown, 1000, 4600, 70, 69, &[]);
        assert_eq!(u1h.rate_pct_per_hour(), None);
        // 零/负时长（时钟回拨）仍不产生速率。
        let s0 = seg("d1", SegmentType::Active, 1000, 1000, 71, 70, &[]);
        assert_eq!(s0.rate_pct_per_hour(), None);
    }

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
        let csv = render_csv(&segs, None, CST, 1_760_000_000);
        let mut lines = csv.lines();
        let first = lines.next().unwrap();
        assert!(first.starts_with('\u{feff}'), "BOM 缺失");
        assert!(
            first[3..].starts_with("# manifest.schema_version="),
            "BOM 后应为 manifest 注释块: {first}"
        );
        // 跳过 `#` 注释续行后应为表头。
        let header = lines.find(|l| !l.starts_with("# ")).unwrap();
        assert_eq!(header, CSV_HEADER);
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
        let csv = render_csv(&[], None, CST, 1_760_000_000);
        assert_eq!(
            csv,
            format!(
                "\u{feff}{}{CSV_HEADER}\n",
                crate::export_manifest::manifest_csv_comments(1_760_000_000)
            )
        );
        // 行序 = 展示键字典序（匿名化后按 D 编号，哈希序 dev-1(D1) 先于 dev-0(D2)）。
        let segs = vec![
            seg("dev-0", SegmentType::Active, 0, 3600, 100, 99, &[]),
            seg("dev-1", SegmentType::Active, 0, 3600, 100, 99, &[]),
        ];
        let keys: BTreeSet<String> = ["dev-0", "dev-1"].iter().map(|s| s.to_string()).collect();
        let anon = anonymize_map(&keys);
        let csv = render_csv(&segs, Some(&anon), CST, 1_760_000_000);
        let rows: Vec<&str> = csv
            .lines()
            .filter(|l| !l.starts_with("# "))
            .skip(1)
            .collect();
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

    // -- 自描述 manifest（组5 N-12② · 5a）------------------------------------

    /// A 要素防漂移：统计 JSON 顶层键集合 == STATS_JSON_TOP_FIELDS。
    #[test]
    fn json_export_top_fields_match_manifest_dictionary() {
        let events = vec![ev_poll(1000, "kbd-1", 90)];
        let v: serde_json::Value =
            serde_json::from_str(&render_json_events(&events, None, 42)).unwrap();
        let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        keys.sort_unstable();
        let mut dict = crate::export_manifest::STATS_JSON_TOP_FIELDS.to_vec();
        dict.sort_unstable();
        assert_eq!(keys, dict);
        // manifest 透出且生成时刻与导出时刻一致。
        assert_eq!(v["manifest"]["generated_at"], 42);
        assert_eq!(
            v["manifest"]["schema_version"],
            crate::export_manifest::MANIFEST_SCHEMA_VERSION
        );
    }

    /// A 要素防漂移：HTML volttray-data 顶层键集合 == HTML_DATA_TOP_FIELDS。
    #[test]
    fn report_data_top_fields_match_manifest_dictionary() {
        let events = vec![
            StatsEvent::RunStart { ts: 1000 },
            ev_poll(2000, "kbd-1", 90),
            ev_poll(9200, "kbd-1", 88),
        ];
        let segs = build_segments(&events);
        let report = build_report_data(&events, &segs, None, CST, 200000);
        let mut keys: Vec<&str> = report
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        keys.sort_unstable();
        let mut dict = crate::export_manifest::HTML_DATA_TOP_FIELDS.to_vec();
        dict.sort_unstable();
        assert_eq!(keys, dict);
        assert_eq!(report["manifest"]["generated_at"], 200000);
        // BvA-6：manifest 块不含采集时区偏移值（meta.tz_offset 属既有字段，
        // 不在本负面清单范围）。
        let m = report["manifest"].to_string();
        for bad in ["+08:00", "tz_offset"] {
            assert!(!m.contains(bad), "manifest 含负面清单值: {bad}");
        }
    }

    /// CSV 结构钉板：BOM → `#` 注释块 → 表头 → 数据行。
    #[test]
    fn csv_manifest_comment_block_precedes_header() {
        let csv = render_csv(&[], None, CST, 7);
        assert!(csv.starts_with('\u{feff}'));
        let body = &csv[3..];
        let comments: Vec<&str> = body.lines().take_while(|l| l.starts_with("# ")).collect();
        assert_eq!(
            comments.len(),
            crate::export_manifest::manifest_csv_comments(7)
                .lines()
                .count()
        );
        assert!(comments[0].starts_with("# manifest.schema_version="));
        assert!(comments.contains(&"# manifest.generated_at=7"));
        // 注释块之后即表头行。
        let after = body.lines().nth(comments.len()).unwrap();
        assert_eq!(after, CSV_HEADER);
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
        let csv_rows = render_csv(&segs, None, CST, 200000)
            .lines()
            .filter(|l| !l.starts_with("# "))
            .count()
            - 1;
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
        // 融合稿结构：chart2 / recent7d_active_rate / chart2_approx 已移除；
        // 分日视图、首屏 KPI、双口径对账与缺口卡取而代之。
        assert!(report.get("chart2").is_none());
        assert!(report.get("recent7d_active_rate").is_none());
        assert!(report["meta"].get("chart2_approx").is_none());
        assert!(report["meta"].get("recent_days").is_none());
        assert_eq!(report["meta"]["version"].as_i64().unwrap(), 2);
        assert!(report["daily_cards"][k].is_array());
        assert!(report["first_screen_kpis"][k].is_object());
        assert!(report["avg_estimate"][k].is_object());
        assert!(report["week_over_week"][k].is_object());
        assert!(report["activity_gap_note"]["present"].as_bool().unwrap());
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
        assert!(c1["sticky_spans"].is_array());
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
        let csv = render_csv(&segs, None, CST, base + DAYS * 86_400);
        assert_eq!(
            csv.lines().filter(|l| !l.starts_with("# ")).count() - 1,
            segs.len(),
            "CSV 行数 = 段数"
        );
        let report = build_report_data(&events, &segs, None, CST, base + DAYS * 86_400);
        assert_eq!(report["segments"].as_array().unwrap().len(), segs.len());
        let html = render_html(&report);
        assert!(html.contains(r#"<script id="volttray-data" type="application/json">"#));
        assert!(html.len() > 10_000);
        // 段数应远小于事件数（coalesce 生效），量级合理。
        assert!(segs.len() < events.len() / 10, "段数 {}", segs.len());
    }

    /// G-28：HTML 模板含读数年龄渲染钩子（ageText / ageStale / 超 2h 徽标），
    /// 且年龄基准（generated_at）与读数时刻（current_ts）golden 透出。
    #[test]
    fn g28_html_template_has_reading_age_hooks() {
        let k = "kbd-1";
        let events = vec![
            StatsEvent::RunStart { ts: 1000 },
            ev_poll(2000, k, 90),
            ev_poll(9200, k, 88),
        ];
        let segs = build_segments(&events);
        let report = build_report_data(&events, &segs, None, CST, 200000);
        assert_eq!(report["meta"]["generated_at"].as_i64().unwrap(), 200000);
        assert_eq!(
            report["first_screen_kpis"][k]["current_ts"]
                .as_i64()
                .unwrap(),
            9200
        );
        let html = render_html(&report);
        assert!(html.contains("function ageText("), "缺 ageText 钩子");
        assert!(html.contains("function ageStale("), "缺 ageStale 钩子");
        assert!(html.contains("更新于 "), "缺 KPI 卡年龄文案钩子");
        assert!(html.contains("读数超 2h 未刷新"), "缺超 2h 醒目徽标钩子");
    }
}

#[cfg(test)]
mod filename_tests {
    use super::tests::{ev_poll, ev_read, seg, CST};
    use super::*;
    use crate::diag_export::LocalTimeParts;
    use crate::stats::{ActivityDay, SOURCE_POLL};

    // -------------------------------------------------------------------
    // 融合稿 Phase 0 测试网（13 项，钉死新口径与删除守卫）
    // -------------------------------------------------------------------

    /// 测试锚定日界：取固定锚点的本地日界，day 偏移以天计。
    fn day_base() -> i64 {
        local_day_start(1_760_000_000, CST)
    }

    fn card(date: &str, drain: i16, slope: Option<f64>, sticky: bool, sparse: bool) -> DailyCard {
        DailyCard {
            date: date.to_string(),
            first_ts: 0,
            first_pct: 90,
            last_ts: 0,
            last_pct: (90 - drain).max(0) as u8,
            drain_pct: drain,
            charge_recovery_pct: 0,
            reads: 3,
            coverage_secs: 7200,
            slope_pct_per_hour: slope,
            sticky,
            sparse,
            charging_spans: Vec::new(),
            unknown_spans: Vec::new(),
        }
    }

    #[test]
    fn daily_card_first_last_delta() {
        let d0 = day_base();
        let events = vec![
            ev_poll(d0 + 8 * 3600, "d1", 90),
            ev_poll(d0 + 12 * 3600, "d1", 88),
            // 充电态快照不入放电首末差。
            ev_read(d0 + 14 * 3600, "d1", 91, true, SOURCE_POLL),
            ev_poll(d0 + 20 * 3600, "d1", 85),
            ev_poll(d0 + 86400 + 9 * 3600, "d1", 84),
            ev_poll(d0 + 86400 + 18 * 3600, "d1", 82),
        ];
        let segs = build_segments(&events);
        let sd = sticky_day_set(&sticky_spans(&segs), CST);
        let cards = build_daily_cards(&events, &segs, &sd, CST);
        let v = &cards["d1"];
        assert_eq!(v.len(), 2);
        let d0s = fmt_local_date(d0 + 8 * 3600, CST);
        let d1s = fmt_local_date(d0 + 86400 + 9 * 3600, CST);
        assert_eq!(v[0].date, d0s);
        assert_eq!(v[0].first_pct, 90);
        assert_eq!(v[0].last_pct, 85);
        assert_eq!(v[0].drain_pct, 5);
        assert_eq!(v[0].reads, 3);
        assert_eq!(v[0].coverage_secs, 12 * 3600);
        assert_eq!(v[0].slope_pct_per_hour, Some(round2(5.0 / 12.0)));
        assert!(!v[0].sparse);
        assert!(!v[0].sticky);
        assert_eq!(v[1].date, d1s);
        assert_eq!(v[1].drain_pct, 2);
    }

    #[test]
    fn daily_card_sticky_flag() {
        let d0 = day_base();
        // >24h 零斜率放电段 → 覆盖日均打「设备上报粘滞」。
        let segs = vec![seg(
            "d1",
            SegmentType::Active,
            d0 + 3600,
            d0 + 3600 + 90_000,
            86,
            86,
            &[],
        )];
        let spans = sticky_spans(&segs);
        assert_eq!(spans["d1"].len(), 1);
        let sd = sticky_day_set(&spans, CST);
        let days = &sd["d1"];
        assert!(days.contains(&fmt_local_date(d0 + 7200, CST)));
        assert!(days.contains(&fmt_local_date(d0 + 86400 + 3600, CST)));
        // 不足 24h 的零斜率段不算粘滞。
        let short = vec![seg("d1", SegmentType::Active, d0, d0 + 3600, 86, 86, &[])];
        assert!(sticky_spans(&short).is_empty());
        // 日卡透传粘滞标签。
        let events = vec![
            ev_poll(d0 + 7200, "d1", 86),
            ev_poll(d0 + 8 * 3600, "d1", 86),
            ev_poll(d0 + 16 * 3600, "d1", 85),
        ];
        let cards = build_daily_cards(&events, &segs, &sd, CST);
        assert!(cards["d1"][0].sticky);
    }

    #[test]
    fn daily_card_sparse_no_rate() {
        let d0 = day_base();
        // read<3 → 样本不足，不出斜率。
        let e2 = vec![
            ev_poll(d0 + 3600, "d1", 90),
            ev_poll(d0 + 8 * 3600, "d1", 85),
        ];
        let segs2 = build_segments(&e2);
        let sd2 = sticky_day_set(&sticky_spans(&segs2), CST);
        let c2 = build_daily_cards(&e2, &segs2, &sd2, CST);
        assert!(c2["d1"][0].sparse);
        assert_eq!(c2["d1"][0].slope_pct_per_hour, None);
        assert_eq!(c2["d1"][0].drain_pct, 5); // 首末差照常给
                                              // read≥3 但覆盖<2h → 同样样本不足。
        let e3 = vec![
            ev_poll(d0 + 3600, "d1", 90),
            ev_poll(d0 + 4200, "d1", 89),
            ev_poll(d0 + 4800, "d1", 88),
        ];
        let segs3 = build_segments(&e3);
        let sd3 = sticky_day_set(&sticky_spans(&segs3), CST);
        let c3 = build_daily_cards(&e3, &segs3, &sd3, CST);
        assert!(c3["d1"][0].sparse);
        assert_eq!(c3["d1"][0].slope_pct_per_hour, None);
    }

    #[test]
    fn avg_estimate_median_robust() {
        // 斜率样本 [2.0, 0.5]（粘滞日 0.0 已被剔除）→ 中位 1.25 → 上界 30.0。
        let d0 = day_base();
        let events = vec![ev_poll(d0, "d1", 90), ev_poll(d0 + 3 * 86400, "d1", 80)];
        let cards = vec![
            card(&fmt_local_date(d0, CST), 5, Some(2.0), false, false),
            card(&fmt_local_date(d0 + 86400, CST), 0, Some(0.0), true, false),
            card(
                &fmt_local_date(d0 + 2 * 86400, CST),
                2,
                Some(0.5),
                false,
                false,
            ),
        ];
        let mut sticky = BTreeSet::new();
        sticky.insert(fmt_local_date(d0 + 86400, CST));
        let est = build_avg_estimate(&events, &[], "d1", &cards, &sticky);
        assert_eq!(est.median_slope_pct_per_hour, Some(1.25));
        assert_eq!(est.upper_pct_per_day, Some(30.0));
        assert_eq!(est.valid_days, 2);
        // 下界 = 总账 (90-80)/3 天 ≈ 3.33；30/3.33 > 2 → 口径冲突。
        assert!(est.conflict);
        assert!(est.badges.contains(&"样本不足".to_string()));
        assert!(est.badges.contains(&"含粘滞".to_string()));
        assert!(est.badges.contains(&"口径冲突".to_string()));
    }

    #[test]
    fn rate_guard_short_segment() {
        // 钉 -400.00 伪值回归：9s 段 71→70（原口径 -400%/h）→ None。
        let s9 = seg("d1", SegmentType::Active, 1000, 1009, 71, 70, &[]);
        assert_eq!(s9.rate_pct_per_hour(), None);
        // 5min 边界：299s 拦、300s 放。
        let s299 = seg("d1", SegmentType::Sleep, 1000, 1299, 71, 70, &[]);
        assert_eq!(s299.rate_pct_per_hour(), None);
        let s300 = seg("d1", SegmentType::Sleep, 1000, 1300, 71, 70, &[]);
        assert_eq!(s300.rate_pct_per_hour(), Some(-12.0));
        // |Δ%|<1（零 delta）不论时长 → None。
        let z = seg("d1", SegmentType::Active, 1000, 100000, 86, 86, &[]);
        assert_eq!(z.rate_pct_per_hour(), None);
    }

    #[test]
    fn activity_gap_note_present() {
        // 无 activity_buckets 事件 → 缺口卡必然出现。
        let events = vec![ev_poll(1000, "d1", 90), ev_poll(8200, "d1", 88)];
        let segs = build_segments(&events);
        let report = build_report_data(&events, &segs, None, CST, 9000);
        let gap = &report["activity_gap_note"];
        assert!(gap["present"].as_bool().unwrap());
        assert!(gap["text"].as_str().unwrap().contains("尚无静止/活动字段"));
        assert_eq!(report["meta"]["activity_days"].as_i64().unwrap(), 0);
        // 位图积累 ≥7 天 → 缺口闭合标记翻转。
        let mkday = |d: u8| ActivityDay {
            date: format!("2026-10-{d:02}"),
            bitmap_hex: "0".repeat(crate::stats::ACTIVITY_BITMAP_HEX_LEN),
        };
        let days7: Vec<ActivityDay> = (18..=24).map(mkday).collect();
        let ev2 = vec![
            ev_poll(1000, "d1", 90),
            ev_poll(8200, "d1", 88),
            StatsEvent::ActivityBuckets {
                ts: 8300,
                physical_key: "d1".into(),
                days: days7,
            },
        ];
        let report2 = build_report_data(&ev2, &segs, None, CST, 9000);
        assert!(!report2["activity_gap_note"]["present"].as_bool().unwrap());
        assert_eq!(report2["meta"]["activity_days"].as_i64().unwrap(), 7);
    }

    #[test]
    fn app_run_timeline_not_mouse_activity() {
        let events = vec![
            StatsEvent::RunStart { ts: 1000 },
            ev_poll(2000, "d1", 90),
            ev_poll(9200, "d1", 88),
            StatsEvent::RunStop { ts: 10000 },
        ];
        let segs = build_segments(&events);
        let report = build_report_data(&events, &segs, None, CST, 20000);
        let html = render_html(&report);
        assert!(
            html.contains("app 运行时段"),
            "时间轴标题/脚注须含 app 运行时段"
        );
        assert!(!html.contains("鼠标使用时间"), "禁用误导性表述");
    }

    #[test]
    fn avg_estimate_dual_ledger_conflict() {
        // 上界 24.0（中位 1.0×24）vs 下界 0.5（5%/10 天）→ 倍差 48 > 2 → 冲突，
        // 且区间形态不变（lower/upper 照常成对给出）。
        let d0 = day_base();
        let events = vec![ev_poll(d0, "d1", 90), ev_poll(d0 + 10 * 86400, "d1", 85)];
        let cards = vec![
            card(&fmt_local_date(d0, CST), 3, Some(1.0), false, false),
            card(&fmt_local_date(d0 + 86400, CST), 2, Some(1.0), false, false),
        ];
        let est = build_avg_estimate(&events, &[], "d1", &cards, &BTreeSet::new());
        assert!(est.conflict);
        assert!(est.badges.contains(&"口径冲突".to_string()));
        assert_eq!(est.upper_pct_per_day, Some(24.0));
        assert!(est.lower_pct_per_day.is_some());
    }

    #[test]
    fn avg_estimate_sticky_days_excluded() {
        // 粘滞日的极端斜率不进中位数样本：剔除后中位 1.0 → 上界 24.0；
        // 若未剔除，[1.0, 50.0] 中位将是 25.5。
        let d0 = day_base();
        let events = vec![ev_poll(d0, "d1", 90), ev_poll(d0 + 2 * 86400, "d1", 80)];
        let cards = vec![
            card(&fmt_local_date(d0, CST), 3, Some(1.0), false, false),
            card(
                &fmt_local_date(d0 + 86400, CST),
                10,
                Some(50.0),
                true,
                false,
            ),
        ];
        let mut sticky = BTreeSet::new();
        sticky.insert(fmt_local_date(d0 + 86400, CST));
        let est = build_avg_estimate(&events, &[], "d1", &cards, &sticky);
        assert_eq!(est.median_slope_pct_per_hour, Some(1.0));
        assert_eq!(est.upper_pct_per_day, Some(24.0));
        assert_eq!(est.valid_days, 1);
        assert_eq!(est.sticky_days, 1);
    }

    #[test]
    fn avg_estimate_zero_slope_valid_day_included() {
        // 复核批 F1 回归：零斜率的非粘滞有效日必须计入中位数样本——
        // 规格 §3③ 仅授权剔除「粘滞标记日」。样本 [1.0, 0.0]：
        // 该日计入 valid_days，且中位数被其拉低至 0.5（旧的
        // *v > 0.0 过滤口径下 valid_days=1、中位 1.0）。
        let d0 = day_base();
        let events = vec![ev_poll(d0, "d1", 90), ev_poll(d0 + 3 * 86400, "d1", 80)];
        let cards = vec![
            card(&fmt_local_date(d0, CST), 3, Some(1.0), false, false),
            card(&fmt_local_date(d0 + 86400, CST), 0, Some(0.0), false, false),
        ];
        let est = build_avg_estimate(&events, &[], "d1", &cards, &BTreeSet::new());
        assert_eq!(est.valid_days, 2, "0 斜率有效日须计入样本");
        assert_eq!(est.median_slope_pct_per_hour, Some(0.5));
        assert_eq!(est.upper_pct_per_day, Some(12.0));
    }

    #[test]
    fn activity_days_cross_event_date_union() {
        // 复核批 F2 回归：activity_days = 跨事件按 date 去重并集。
        // 事件 A 携带 {D1, D2}、事件 B 携带 {D2, D3}（boundary 冲刷把
        // 同一日散入多事件的典型形态）→ 并集 {D1,D2,D3} = 3；
        // 旧「单事件最大值」口径给出 2，漏计 D3。
        let mkday = |d: u8| ActivityDay {
            date: format!("2026-10-{d:02}"),
            bitmap_hex: "0".repeat(crate::stats::ACTIVITY_BITMAP_HEX_LEN),
        };
        let events = vec![
            ev_poll(1000, "d1", 90),
            ev_poll(8200, "d1", 88),
            StatsEvent::ActivityBuckets {
                ts: 8300,
                physical_key: "d1".into(),
                days: vec![mkday(22), mkday(23)],
            },
            StatsEvent::ActivityBuckets {
                ts: 8400,
                physical_key: "d1".into(),
                days: vec![mkday(23), mkday(24)],
            },
        ];
        let segs = build_segments(&events);
        let report = build_report_data(&events, &segs, None, CST, 9000);
        assert_eq!(report["meta"]["activity_days"].as_i64().unwrap(), 3);
    }

    #[test]
    fn first_screen_no_inferred_single_value() {
        // 首屏推断项断言为区间形态：avg 只有 lower/upper 成对字段 +
        // conflict/badges，不存在任何单值字段。
        let events = vec![
            StatsEvent::RunStart { ts: 1000 },
            ev_poll(2000, "d1", 90),
            ev_poll(9200, "d1", 88),
            StatsEvent::RunStop { ts: 10000 },
        ];
        let segs = build_segments(&events);
        let report = build_report_data(&events, &segs, None, CST, 20000);
        let avg = report["first_screen_kpis"]["d1"]["avg"]
            .as_object()
            .unwrap();
        let keys: Vec<&str> = avg.keys().map(|k| k.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "badges",
                "conflict",
                "lower_pct_per_day",
                "upper_pct_per_day"
            ]
        );
        for banned in ["value", "estimate", "rate", "pct_per_day", "point"] {
            assert!(!avg.contains_key(banned), "首屏推断项禁单值字段: {banned}");
        }
    }

    #[test]
    fn next_charge_estimate_absent() {
        // 删除守卫：该概念不以任何形式出现在报告任何位置。
        let events = vec![
            StatsEvent::RunStart { ts: 1000 },
            ev_poll(2000, "d1", 90),
            ev_poll(9200, "d1", 88),
            StatsEvent::RunStop { ts: 10000 },
        ];
        let segs = build_segments(&events);
        let report = build_report_data(&events, &segs, None, CST, 20000);
        let html = render_html(&report);
        assert!(!html.contains("距下次充电"));
        assert!(!report.to_string().contains("距下次充电"));
    }

    #[test]
    fn chart2_approx_removed() {
        // 跨日摊入口径图不存在：数据块与渲染产物均无 chart2 残留。
        let events = vec![
            StatsEvent::RunStart { ts: 1000 },
            ev_poll(2000, "d1", 90),
            ev_poll(9200, "d1", 88),
            StatsEvent::RunStop { ts: 10000 },
        ];
        let segs = build_segments(&events);
        let report = build_report_data(&events, &segs, None, CST, 20000);
        assert!(report.get("chart2").is_none());
        assert!(report["meta"].get("chart2_approx").is_none());
        let html = render_html(&report);
        assert!(!html.contains("chart2_approx"));
        assert!(!html.contains("跨日段线性摊入（近似）"));
    }

    #[test]
    fn week_over_week_suppressed_on_sticky() {
        let d0 = day_base();
        let mk = |i: i64, sticky: bool| {
            card(
                &fmt_local_date(d0 + i * 86400, CST),
                2,
                Some(0.5),
                sticky,
                false,
            )
        };
        // 近 7 天含粘滞 → 周环比抑制、显示「不可比」。
        let mut cards: Vec<DailyCard> = (0..14).map(|i| mk(i, false)).collect();
        cards[10].sticky = true;
        let wow = build_week_over_week(&cards, false);
        assert!(wow.suppressed);
        assert_eq!(wow.reason.as_deref(), Some("不可比"));
        assert_eq!(wow.delta_pct_per_day, None);
        // 口径冲突同样抑制。
        let clean: Vec<DailyCard> = (0..14).map(|i| mk(i, false)).collect();
        let wow2 = build_week_over_week(&clean, true);
        assert!(wow2.suppressed);
        // 无粘滞无冲突 → 正常给箭头值（等值日耗电 → delta 0）。
        let wow3 = build_week_over_week(&clean, false);
        assert!(!wow3.suppressed);
        assert_eq!(wow3.delta_pct_per_day, Some(0.0));
        // 前 7 天缺失 → 样本不足。
        let few: Vec<DailyCard> = (0..5).map(|i| mk(i, false)).collect();
        let wow4 = build_week_over_week(&few, false);
        assert!(wow4.suppressed);
        assert_eq!(wow4.reason.as_deref(), Some("样本不足"));
    }

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

    // -------------------------------------------------------------------
    // RB2 修复落实批 FR-2/3/4/5/8 验证测试（E 签发终审稿）
    // -------------------------------------------------------------------

    /// FR-2：10 日样本（8 非负斜率 + 2 负斜率），断言 valid_days=8、
    /// upper 非 None、负斜率两日不进样本；零斜率日仍在样本。
    #[test]
    fn fr2_negative_slope_days_excluded_from_median() {
        let d0 = day_base();
        let mut cards: Vec<DailyCard> = Vec::new();
        for i in 0..10 {
            let slope = match i {
                // 两个隔夜充电净增益日：合法负斜率。
                3 => Some(-0.4),
                7 => Some(-0.6),
                // 五个正常放电日 + 三个零斜率日（非粘滞）。
                _ if i < 5 => Some(0.5 + i as f64 * 0.1),
                _ => Some(0.0),
            };
            cards.push(card(
                &fmt_local_date(d0 + i * 86400, CST),
                2,
                slope,
                false,
                false,
            ));
        }
        let avg = build_avg_estimate(&[], &[], "d1", &cards, &BTreeSet::new());
        assert_eq!(avg.valid_days, 8);
        assert!(avg.upper_pct_per_day.is_some());
        // 8 样本排序 = [0,0,0,0,0.5,0.6,0.7,0.9]，中位 = (0+0.5)/2 = 0.25。
        assert_eq!(avg.median_slope_pct_per_hour, Some(0.25));
        assert_eq!(avg.upper_pct_per_day, Some(6.0));
        // 无 events/segments → ledger None → 不触发冲突。
        assert!(!avg.conflict);
        // 回归锚：零斜率日仍在样本（valid_days 已含 3 个零斜率日）。
        let zero_only: Vec<DailyCard> = (0..10)
            .map(|i| {
                card(
                    &fmt_local_date(d0 + i * 86400, CST),
                    2,
                    Some(0.0),
                    false,
                    false,
                )
            })
            .collect();
        let avg2 = build_avg_estimate(&[], &[], "d1", &zero_only, &BTreeSet::new());
        assert_eq!(avg2.valid_days, 10);
        assert_eq!(avg2.median_slope_pct_per_hour, Some(0.0));
    }

    /// FR-3：conflict 三验证用例 + 既有分支回归。
    #[test]
    fn fr3_conflict_bidirectional_and_u0_guard() {
        // 终审稿验证：median_slope=0/ledger=0.9 → false。
        assert!(!avg_conflict(Some(0.9), Some(0.0)));
        // 终审稿验证：ledger=12/u=4 → true（l 远超 u 的倒挂区间）。
        assert!(avg_conflict(Some(12.0), Some(4.0)));
        // 终审稿验证：u=-1 → true（任一为负直接置真）。
        assert!(avg_conflict(Some(2.0), Some(-1.0)));
        assert!(avg_conflict(Some(-0.5), Some(2.0)));
        // u=0 退化绝对阈值：l>1.0 才置真。
        assert!(avg_conflict(Some(1.5), Some(0.0)));
        assert!(!avg_conflict(Some(1.0), Some(0.0)));
        // 既有上臂语义保持（N-4 回归锚）：u > 2l。
        assert!(avg_conflict(Some(1.0), Some(2.5)));
        assert!(!avg_conflict(Some(1.0), Some(2.0)));
        // l<=0 且 u>0 保持置真。
        assert!(avg_conflict(Some(0.0), Some(1.0)));
        // 双方均为 0 → 不冲突。
        assert!(!avg_conflict(Some(0.0), Some(0.0)));
        // 任一侧 None → 不冲突。
        assert!(!avg_conflict(None, Some(1.0)));
        assert!(!avg_conflict(Some(1.0), None));
    }

    /// FR-4：崩溃+重启时 RunStart 覆盖未闭合 run，以「上一会话最后
    /// 一个事件 ts」闭合，产出段不跨 4 天；open 后无事件则丢弃。
    #[test]
    fn fr4_crashed_run_closed_at_last_event_of_session() {
        let d0 = day_base();
        let events = vec![
            StatsEvent::RunStart { ts: d0 + 9 * 3600 },
            ev_read(d0 + 10 * 3600, "d1", 80, false, SOURCE_POLL),
            StatsEvent::RunStart {
                ts: d0 + 4 * 86400 + 9 * 3600,
            },
            ev_read(d0 + 4 * 86400 + 10 * 3600, "d1", 79, false, SOURCE_POLL),
        ];
        let spans = build_run_spans_by_day(&events, CST);
        // 两段各一小时：周一崩溃段 + 周五末段——而非跨 4 天的段。
        assert_eq!(spans.len(), 2);
        let day1 = fmt_local_date(d0 + 9 * 3600, CST);
        assert_eq!(spans[&day1], vec![(d0 + 9 * 3600, d0 + 10 * 3600)]);
        // 周五会话末尾未闭合但有事件 → 以最后事件闭合。
        let day5 = fmt_local_date(d0 + 4 * 86400 + 9 * 3600, CST);
        assert_eq!(
            spans[&day5],
            vec![(d0 + 4 * 86400 + 9 * 3600, d0 + 4 * 86400 + 10 * 3600)]
        );
        // 崩溃段 open 之后无任何事件 → 丢弃不开段。
        let lonely = vec![
            StatsEvent::RunStart { ts: d0 + 9 * 3600 },
            StatsEvent::RunStop { ts: d0 + 10 * 3600 },
            StatsEvent::RunStart {
                ts: d0 + 4 * 86400 + 9 * 3600,
            },
            StatsEvent::RunStart {
                ts: d0 + 8 * 86400 + 9 * 3600,
            },
        ];
        let spans2 = build_run_spans_by_day(&lonely, CST);
        // 仅第一对 RunStart/RunStop 成段；后两个孤儿 RunStart 均丢弃。
        assert_eq!(spans2.len(), 1);
        assert_eq!(spans2[&day1], vec![(d0 + 9 * 3600, d0 + 10 * 3600)]);
    }

    /// FR-5：周末党 14 日样本（隔 5 天缺口）——suppressed=false、
    /// delta 非 None、挂「窗口含缺口」徽标、窗口起止正确；含 1 个
    /// sparse 日窗口分母排除该日。
    #[test]
    fn fr5_wow_window_annotation_sparse_removal_gap_badge() {
        let d0 = day_base();
        let date = |i: i64| fmt_local_date(d0 + i * 86400, CST);
        // 周末党：每个周末 2 天（0,1 / 7,8 / … / 42,43），14 张卡，
        // 相邻卡间隔 5 天 → 日历缺口 >2 天。
        let weekend_days: Vec<i64> = (0..7).flat_map(|w| vec![w * 7, w * 7 + 1]).collect();
        assert_eq!(weekend_days.len(), 14);
        let cards: Vec<DailyCard> = weekend_days
            .iter()
            .map(|&i| {
                card(
                    &date(i),
                    if i >= 22 { 6 } else { 10 },
                    Some(0.3),
                    false,
                    false,
                )
            })
            .collect();
        let wow = build_week_over_week(&cards, false);
        assert!(!wow.suppressed);
        // recent（22..43）日均 6，prev（0..21）日均 10 → delta=-4。
        assert_eq!(wow.delta_pct_per_day, Some(-4.0));
        assert!(wow.badges.iter().any(|b| b == "窗口含缺口"));
        assert_eq!(wow.window_first.as_deref(), Some(date(0).as_str()));
        assert_eq!(wow.window_last.as_deref(), Some(date(43).as_str()));
        // 连续 14 天无缺口 → 无徽标。
        let contig: Vec<DailyCard> = (0..14)
            .map(|i| card(&date(i), 4, Some(0.3), false, false))
            .collect();
        let wow2 = build_week_over_week(&contig, false);
        assert!(wow2.badges.is_empty());
        // sparse 日剔除出均值分母：recent 中 1 个 sparse 日（drain 100
        // 拉偏值）被排除，delta = 0 而非被拉高。
        let mut sparse_cards: Vec<DailyCard> = (0..14)
            .map(|i| card(&date(i), 4, Some(0.3), false, false))
            .collect();
        sparse_cards[10].sparse = true;
        sparse_cards[10].drain_pct = 100;
        let wow3 = build_week_over_week(&sparse_cards, false);
        assert!(!wow3.suppressed);
        assert_eq!(wow3.delta_pct_per_day, Some(0.0));
        // recent 全 sparse → 样本不足。
        let mut all_sparse: Vec<DailyCard> = (0..14)
            .map(|i| card(&date(i), 4, Some(0.3), false, false))
            .collect();
        for c in all_sparse.iter_mut().skip(7) {
            c.sparse = true;
        }
        let wow4 = build_week_over_week(&all_sparse, false);
        assert!(wow4.suppressed);
        assert_eq!(wow4.reason.as_deref(), Some("样本不足"));
    }

    /// FR-8：跨 DST 切换（单一 UTC 偏移 ±1h）时，事件日归属偏差
    /// ≤1 天，与页脚文案「≤1h 归属偏差」一致；页脚文案已渲染。
    #[test]
    fn fr8_dst_day_attribution_shift_within_one_hour() {
        let d0 = day_base();
        let events = vec![
            ev_poll(d0 + 8 * 3600, "d1", 90),
            // 本地 23:30 的事件：偏移 +1h（DST 切换）后落到次日 00:30。
            ev_poll(d0 + 23 * 3600 + 1800, "d1", 85),
            ev_poll(d0 + 86400 + 8 * 3600, "d1", 80),
            ev_poll(d0 + 86400 + 20 * 3600, "d1", 75),
        ];
        let ts_list: Vec<i64> = vec![
            d0 + 8 * 3600,
            d0 + 23 * 3600 + 1800,
            d0 + 86400 + 8 * 3600,
            d0 + 86400 + 20 * 3600,
        ];
        // 每个事件在两个偏移下的日归属差 ≤1 天（与文案 ≤1h 一致）。
        for &ts in &ts_list {
            let a = civil_days(&fmt_local_date(ts, CST)).unwrap();
            let b = civil_days(&fmt_local_date(ts, CST + 60)).unwrap();
            assert!((b - a).abs() <= 1, "day shift > 1 for ts {ts}");
        }
        // 23:30 事件恰好被 1h 偏移推到次日。
        let late = d0 + 23 * 3600 + 1800;
        let a = civil_days(&fmt_local_date(late, CST)).unwrap();
        let b = civil_days(&fmt_local_date(late, CST + 60)).unwrap();
        assert_eq!(b - a, 1);
        // build_daily_cards 双偏移下日集合大小一致（2 天）。
        let segs = build_segments(&events);
        let sd = sticky_day_set(&sticky_spans(&segs), CST);
        let c1 = build_daily_cards(&events, &segs, &sd, CST);
        let c2 = build_daily_cards(&events, &segs, &sd, CST + 60);
        assert_eq!(c1["d1"].len(), 2);
        assert_eq!(c2["d1"].len(), 2);
        // 页脚文案在模板中（单一 UTC 偏移 + ≤1h 偏差 + 不重算声明）。
        assert!(HTML_TEMPLATE.contains("日界按单一 UTC 偏移计算"));
        assert!(HTML_TEMPLATE.contains("跨夏令时切换日前后历史日存在 ≤1h 归属偏差"));
        assert!(HTML_TEMPLATE.contains("不做事件级时区库重算"));
    }
}
