//! 唤醒信号通道归因（v3 A-3）：Raw Input 设备路径 → HID 通道匹配。
//!
//! Raw Input 的 `hDevice` 经 `GetRawInputDeviceInfoW(RIDI_DEVICENAME)`
//! 得到设备路径；同一接收器（Unifying/Bolt/Lightspeed）后的鼠标路径
//! 与接收器 HID 通道路径共享 VID/PID 与父实例前缀，据此把唤醒信号
//! 归因到通道，只给该通道上的休眠设备装填探测资格。
//!
//! 匹配规则（保守 + E22 单候选回退）：VID/PID 对 + 实例段前两个 `&`
//! 分量全部相等才算同通道；**唯一匹配才返回**——≥2 个候选命中一律
//! None（调用方回退全量装填 + miss 计数，「漏唤醒的损失大于多探测
//! 一次」）。E22 回退：完整指纹零匹配时，若候选集**仅 1 个通道**且
//! VID/PID 级一致即判 Hit——单接收器机器上实例前缀差异（换 USB 口、
//! 拓扑变化）不应让归因失效；多候选时维持实例前缀要求，不回退。
//!
//! 适用边界（v3 A-3 写明）：本机制面向接收器多通道场景治本；蓝牙 /
//! 有线直连（BTHHID / 直连 HID）的设备路径系统性匹配不上接收器
//! 通道，恒定 miss → 回退全量装填，对该批用户不劣化也不改善。

/// 从 Windows 设备路径提取通道指纹（VID/PID 对 + 实例段前两个分量）。
///
/// 路径形如
/// `\\?\HID#VID_046D&PID_C52B&MI_01&Col02#8&1a2b3c4d&0&0001#{guid}`：
/// `#` 分段后第 2 段含 VID/PID（可能带 MI/Col 后缀），第 3 段是
/// 实例 id（`&` 分隔，前两个分量标识父设备/拓扑位置）。任一部分
/// 缺失即 None（不可归因）。
fn channel_fingerprint(path: &str) -> Option<(String, String)> {
    let upper = path.to_uppercase();
    let mut segs = upper.split('#');
    segs.next()?; // \\?\HID
    let ids = segs.next()?;
    let instance = segs.next()?;
    // VID_xxxx 与 PID_xxxx 必须同时存在（允许 MI_/Col 等后缀分量）。
    let mut vid = None;
    let mut pid = None;
    for part in ids.split('&') {
        if let Some(v) = part.strip_prefix("VID_") {
            vid = Some(v);
        } else if let Some(p) = part.strip_prefix("PID_") {
            pid = Some(p);
        }
    }
    let (vid, pid) = (vid?, pid?);
    // 实例段前两个 `&` 分量（不足两个则整段——单分量实例仍可用，
    // 唯一性判定兜底）。
    let mut iparts = instance.split('&');
    let i0 = iparts.next()?;
    if i0.is_empty() {
        return None;
    }
    let inst = match iparts.next() {
        Some(i1) if !i1.is_empty() => format!("{i0}&{i1}"),
        _ => i0.to_string(),
    };
    Some((format!("VID_{vid}&PID_{pid}"), inst))
}

/// 把 Raw Input 设备路径归因到候选 HID 通道。
///
/// `candidates` 为 `(通道标识, 通道 HID 设备路径)` 列表——通道标识由
/// 调用方定义（槽位下标 / 通道号），返回值即该标识在列表中的下标。
/// 唯一指纹匹配返回 `Some(下标)`；零匹配或多匹配（歧义）返回 None。
pub fn channel_for_raw_path(raw_path: &str, candidates: &[(String, String)]) -> Option<usize> {
    match channel_for_raw_path_classified(raw_path, candidates) {
        ChannelMatch::Hit(i) => Some(i),
        _ => None,
    }
}

/// `channel_for_raw_path` 的分类版本（BF-3 / R-11）：把「零匹配」与
/// 「多匹配歧义」从笼统的 None 中拆开，另区分「指纹不可解析」（非
/// HID 路径 / BTHHID 降级路径），供唤醒遥测 miss 子分类计数。归因
/// 语义与原函数完全一致：Hit 即其 Some 分支，其余三种对应 None。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelMatch {
    /// 唯一指纹命中，值为候选下标。
    Hit(usize),
    /// raw_path 指纹不可解析（非 HID 路径 / BTHHID 等降级路径）。
    NoFingerprint,
    /// 指纹可解析但零候选匹配（设备不在本机接收器后）。
    ZeroMatch,
    /// 多个候选同指纹，歧义不可归因。
    Ambiguous,
}

/// 见 [`ChannelMatch`]。候选构造与匹配规则同 `channel_for_raw_path`。
pub fn channel_for_raw_path_classified(
    raw_path: &str,
    candidates: &[(String, String)],
) -> ChannelMatch {
    let fp = match channel_fingerprint(raw_path) {
        Some(fp) => fp,
        None => return ChannelMatch::NoFingerprint,
    };
    let mut hit = None;
    for (i, (_, hid_path)) in candidates.iter().enumerate() {
        if channel_fingerprint(hid_path).as_ref() == Some(&fp) {
            if hit.is_some() {
                return ChannelMatch::Ambiguous; // 歧义：多个候选同指纹
            }
            hit = Some(i);
        }
    }
    match hit {
        Some(i) => ChannelMatch::Hit(i),
        None => {
            // E22 回退：完整指纹零匹配时，候选集仅 1 个通道则放宽到
            // VID/PID 级匹配——单接收器机器上实例前缀差异（换 USB 口 /
            // 拓扑变化）不应让唤醒归因失效；该通道本就是全量装填回退
            // 的唯一受益方，判 Hit 只改善归因与计数、零装填行为变化。
            // 多候选维持实例前缀要求（VID/PID 级无法区分同型号接收器）。
            if candidates.len() == 1 {
                if let Some(cfp) = channel_fingerprint(&candidates[0].1) {
                    if cfp.0 == fp.0 {
                        return ChannelMatch::Hit(0);
                    }
                }
            }
            ChannelMatch::ZeroMatch
        }
    }
}

/// FR-1（RB2）：唤醒信号活动桶键来源。活映射（当前通道槽位
/// unitId）优先 → 持久映射（last_known.json path_map）兜底 → 双 miss
/// 落原路径键；双命中不一致以活映射为准（修-1：活映射为当前真值）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityKeySource {
    /// 活映射命中：unitId 命名空间（与 Read 事件一致）。
    Active,
    /// 持久映射命中（重启空槽位窗口兜底）。
    Persistent,
    /// 双 miss：落原路径键（归因路径已计 channel_map_miss，桶键侧不重复计）。
    RawPathAttributionMiss,
    /// 归因命中通道但槽位无 unitId（身份不明 / 多槽不可归因）：
    /// 桶键侧补计 channel_map_miss（归因路径计不到的 miss）。
    RawPathChannelKeyUnresolved,
}

/// FR-1（RB2）：唤醒信号活动桶键解析——mark 前把 raw path 统一进
/// unitId 命名空间（与 Read 事件同一命名空间），避免键冻结进对外
/// schema 后热力带错位 / 导出幽灵设备 / 接收器场景两鼠标混串。
/// `channel_resolved`：raw path 是否归因命中某通道（决定双 miss 细分）。
pub fn activity_bucket_key(
    active_unit_id: Option<&str>,
    persistent_unit_id: Option<&str>,
    raw_path: &str,
    channel_resolved: bool,
) -> (String, ActivityKeySource) {
    if let Some(u) = active_unit_id.filter(|u| !u.is_empty()) {
        return (u.to_string(), ActivityKeySource::Active);
    }
    if let Some(u) = persistent_unit_id.filter(|u| !u.is_empty()) {
        return (u.to_string(), ActivityKeySource::Persistent);
    }
    if channel_resolved {
        (
            raw_path.to_string(),
            ActivityKeySource::RawPathChannelKeyUnresolved,
        )
    } else {
        (
            raw_path.to_string(),
            ActivityKeySource::RawPathAttributionMiss,
        )
    }
}


// ---------------------------------------------------------------------
// E10（红蓝对抗波二；设计文档 docs/design-e10-wake-attribution.md）：
// 归因两级匹配（VID/PID+MI 放宽）+ 注册期绑定表 + 单函数门控。
// ---------------------------------------------------------------------

/// 放宽指纹：VID/PID 对 + 可选 MI（接口号）。接收器 HID 通道路径通常
/// 不带 MI，Raw Input 鼠标路径常带 MI_01——MI 严格相等会让放宽匹配
/// 系统性失效，故按「双方都有 MI 且不同才不兼容」裁决。
fn relaxed_fingerprint(path: &str) -> Option<(String, Option<String>)> {
    let upper = path.to_uppercase();
    let mut segs = upper.split('#');
    segs.next()?; // \\?\HID
    let ids = segs.next()?;
    let mut vid = None;
    let mut pid = None;
    let mut mi = None;
    for part in ids.split('&') {
        if let Some(v) = part.strip_prefix("VID_") {
            vid = Some(v);
        } else if let Some(p) = part.strip_prefix("PID_") {
            pid = Some(p);
        } else if let Some(m) = part.strip_prefix("MI_") {
            mi = Some(m);
        }
    }
    let (vid, pid) = (vid?, pid?);
    Some((format!("VID_{vid}&PID_{pid}"), mi.map(|m| m.to_string())))
}

/// 设备路径实例段的全部 `&` 分量（注册实例前缀最长公共段消歧用）。
fn instance_parts(path: &str) -> Vec<String> {
    let upper = path.to_uppercase();
    let mut segs = upper.split('#');
    segs.next();
    if segs.next().is_none() {
        return Vec::new();
    }
    match segs.next() {
        Some(inst) => inst.split('&').map(|s| s.to_string()).collect(),
        None => Vec::new(),
    }
}

/// E10（BvA-3）：注册期「通道 → 物理身份」绑定——进程内存态、每次
/// 枚举完成重建、**禁持久化**。`instance_parts` 取自注册时刻观察到
/// 的设备/通道路径实例段，供放宽级候选 >1 时按最长公共前缀消歧。
#[derive(Debug, Clone)]
pub struct WakeBinding {
    /// 通道标识（与候选列表第 1 分量同口径，如槽位键）。
    pub channel_key: String,
    /// 该通道当前注册的物理身份（unitId；未知 → None）。
    pub unit_id: Option<String>,
    /// 注册时刻观察到的实例段分量（大写口径，与 `instance_parts` 一致）。
    pub instance_parts: Vec<String>,
}

impl WakeBinding {
    /// 由通道路径构造绑定（unitId 未知场景；消歧输入即通道自身路径）。
    pub fn from_channel_path(channel_key: String, hid_path: &str) -> Self {
        WakeBinding {
            channel_key,
            unit_id: None,
            instance_parts: instance_parts(hid_path),
        }
    }
}

/// E10 两级归因结果（N-01/N-02）：区分精确命中与放宽命中——遥测
/// `wake_success_hit_exact` / `wake_success_hit_relaxed` 的计数口径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TwoLevelMatch {
    /// 完整指纹（VID/PID + 实例段前两分量）唯一命中。
    HitExact(usize),
    /// 放宽级（VID/PID + MI 兼容）命中——含单候选 VID/PID 回退与
    /// 绑定表注册实例前缀消歧后的命中。
    HitRelaxed(usize),
    /// raw_path 指纹不可解析（非 HID 路径 / BTHHID 等降级路径）。
    NoFingerprint,
    /// 放宽级零候选匹配。
    ZeroMatch,
    /// 放宽级多候选且消歧失败——歧义即 miss（调用方走 arm-all）。
    Ambiguous,
}

/// E10 单函数门控（C 稿「新旧匹配单函数门控」）：新旧归因行为的
/// 唯一切换点。**回滚 = 把本函数体替换为对
/// `channel_for_raw_path_classified` 的直接委托**（代码级回退，
/// 非配置键，见设计文档 §10 回滚表）。
///
/// 匹配顺序：① 精确级（旧规则全等指纹）唯一命中 → HitExact，多
/// 命中 → Ambiguous；② 放宽级（VID/PID + MI 兼容）：零匹配 →
/// ZeroMatch，唯一 → HitRelaxed，多候选 → 按绑定表（缺失回退候选
/// 自身路径）注册实例前缀「最长公共 & 分量段」消歧，唯一最长 →
/// HitRelaxed，否则 Ambiguous（歧义即 miss，调用方回退 arm-all）。
pub fn channel_match_gated(
    raw_path: &str,
    candidates: &[(String, String)],
    bindings: &[WakeBinding],
) -> TwoLevelMatch {
    let fp = match channel_fingerprint(raw_path) {
        Some(fp) => fp,
        None => return TwoLevelMatch::NoFingerprint,
    };
    // ① 精确级（与旧规则同一判定：完整指纹全等、唯一命中）。
    let mut exact = None;
    for (i, (_, hid_path)) in candidates.iter().enumerate() {
        if channel_fingerprint(hid_path).as_ref() == Some(&fp) {
            if exact.is_some() {
                return TwoLevelMatch::Ambiguous;
            }
            exact = Some(i);
        }
    }
    if let Some(i) = exact {
        return TwoLevelMatch::HitExact(i);
    }
    // ② 放宽级：VID/PID 相等且 MI 兼容（双方都有 MI 且不同 → 排除）。
    let (rvp, raw_mi) = match relaxed_fingerprint(raw_path) {
        Some(r) => r,
        None => return TwoLevelMatch::NoFingerprint,
    };
    let mut relaxed: Vec<usize> = Vec::new();
    for (i, (_, hid_path)) in candidates.iter().enumerate() {
        if let Some((cvp, cmi)) = relaxed_fingerprint(hid_path) {
            let mi_ok = match (cmi.as_deref(), raw_mi.as_deref()) {
                (Some(c), Some(r)) => c == r,
                _ => true,
            };
            if cvp == rvp && mi_ok {
                relaxed.push(i);
            }
        }
    }
    match relaxed.len() {
        0 => TwoLevelMatch::ZeroMatch,
        1 => TwoLevelMatch::HitRelaxed(relaxed[0]),
        _ => {
            // 绑定表消歧（BvA-3）：raw 实例段 vs 各候选注册实例段的
            // 最长公共前缀，唯一最长者胜；等长（含全 0）→ 歧义。
            let raw_parts = instance_parts(raw_path);
            let mut best: Option<(usize, usize)> = None; // (候选下标, 公共前缀长)
            let mut tie = false;
            for &i in &relaxed {
                let parts = bindings
                    .iter()
                    .find(|b| b.channel_key == candidates[i].0 && !b.instance_parts.is_empty())
                    .map(|b| b.instance_parts.clone())
                    .unwrap_or_else(|| instance_parts(&candidates[i].1));
                let common = raw_parts
                    .iter()
                    .zip(parts.iter())
                    .take_while(|(a, b)| a == b)
                    .count();
                match best {
                    None => best = Some((i, common)),
                    Some((_, bl)) if common > bl => {
                        best = Some((i, common));
                        tie = false;
                    }
                    Some((_, bl)) if common == bl => tie = true,
                    _ => {}
                }
            }
            match (best, tie) {
                (Some((i, _)), false) => TwoLevelMatch::HitRelaxed(i),
                _ => TwoLevelMatch::Ambiguous,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unifying 接收器 HID 通道（电量查询走的接口）。
    const UNIFYING_CHAN: &str =
        r"\\?\HID#VID_046D&PID_C52B#7&2f3a1b2c&0&0000#{4d1e55b2-f16e-11cf-88cb-001111000030}";
    /// Bolt 接收器 HID 通道。
    const BOLT_CHAN: &str =
        r"\\?\HID#VID_046D&PID_C548#6&9abc0011&0&0000#{4d1e55b2-f16e-11cf-88cb-001111000030}";

    #[test]
    fn unifying_mouse_raw_path_maps_to_its_receiver() {
        // 同一接收器后的鼠标 Raw Input 路径：同 VID/PID、实例段共享
        // 父前缀「7&2f3a1b2c」，带 MI/Col 后缀。
        let raw = r"\\?\HID#VID_046D&PID_C52B&MI_01&Col02#7&2f3a1b2c&0&0001#{378de44c-56ef-11d1-bc8c-00a0c91405dd}";
        let candidates = vec![
            ("bolt".to_string(), BOLT_CHAN.to_string()),
            ("unifying".to_string(), UNIFYING_CHAN.to_string()),
        ];
        assert_eq!(channel_for_raw_path(raw, &candidates), Some(1));
    }

    #[test]
    fn bolt_mouse_raw_path_maps_to_bolt_channel() {
        let raw = r"\\?\HID#VID_046D&PID_C548&MI_02&Col01#6&9abc0011&0&0002#{378de44c-56ef-11d1-bc8c-00a0c91405dd}";
        let candidates = vec![("bolt".to_string(), BOLT_CHAN.to_string())];
        assert_eq!(channel_for_raw_path(raw, &candidates), Some(0));
    }

    #[test]
    fn single_candidate_vid_pid_fallback_hits() {
        // E22 回退：候选集仅 1 个通道时，VID/PID 相同但实例前缀不同
        //（换 USB 口 / 拓扑变化）→ VID/PID 级匹配判 Hit。
        let raw = r"\\?\HID#VID_046D&PID_C52B&MI_01#7&DEADBEEF&0&0001#{378de44c-56ef-11d1-bc8c-00a0c91405dd}";
        let candidates = vec![("unifying".to_string(), UNIFYING_CHAN.to_string())];
        assert_eq!(channel_for_raw_path(raw, &candidates), Some(0));
        assert_eq!(
            channel_for_raw_path_classified(raw, &candidates),
            ChannelMatch::Hit(0)
        );
    }

    #[test]
    fn single_candidate_vid_pid_mismatch_still_zero_match() {
        // E22 回退边界：单候选但 VID/PID 都不同 → 仍 ZeroMatch，不误判。
        let raw = r"\\?\HID#VID_046D&PID_9999&MI_01#7&DEADBEEF&0&0001#{378de44c-56ef-11d1-bc8c-00a0c91405dd}";
        let candidates = vec![("unifying".to_string(), UNIFYING_CHAN.to_string())];
        assert_eq!(channel_for_raw_path(raw, &candidates), None);
        assert_eq!(
            channel_for_raw_path_classified(raw, &candidates),
            ChannelMatch::ZeroMatch
        );
    }

    #[test]
    fn multi_candidate_keeps_instance_requirement() {
        // E22 回退边界：多候选（同型号两台接收器）时实例前缀不同 →
        // 不回退，维持 ZeroMatch（VID/PID 级无法区分两台同型号接收器）。
        let raw = r"\\?\HID#VID_046D&PID_C52B&MI_01#7&DEADBEEF&0&0001#{378de44c-56ef-11d1-bc8c-00a0c91405dd}";
        let candidates = vec![
            ("u1".to_string(), UNIFYING_CHAN.to_string()),
            (
                "u2".to_string(),
                r"\\?\HID#VID_046D&PID_C52B#8&4444aaaa&0&0000#{4d1e55b2-f16e-11cf-88cb-001111000030}"
                    .to_string(),
            ),
        ];
        assert_eq!(channel_for_raw_path(raw, &candidates), None);
        assert_eq!(
            channel_for_raw_path_classified(raw, &candidates),
            ChannelMatch::ZeroMatch
        );
    }

    #[test]
    fn bluetooth_path_systematically_misses() {
        // BTHHID 路径（v3 写明的降级场景）：VID/PID 存在但实例段
        // 与接收器通道无共享前缀 → None（调用方回退全量装填）。
        let raw = r"\\?\BTHHID#{00001124-0000-1000-8000-00805f9b34fb}_VID&0002046d_PID_b01a#9&1c2d3e4f&0&0000#{4d1e55b2-f16e-11cf-88cb-001111000030}";
        let candidates = vec![("unifying".to_string(), UNIFYING_CHAN.to_string())];
        assert_eq!(channel_for_raw_path(raw, &candidates), None);
    }

    #[test]
    fn ambiguous_candidates_yield_none() {
        // 两个候选同指纹（同一路径枚举出多个接口）→ 歧义不可归因。
        let raw = r"\\?\HID#VID_046D&PID_C52B&MI_01#7&2f3a1b2c&0&0001#{378de44c-56ef-11d1-bc8c-00a0c91405dd}";
        let candidates = vec![
            ("a".to_string(), UNIFYING_CHAN.to_string()),
            ("b".to_string(), UNIFYING_CHAN.to_string()),
        ];
        assert_eq!(channel_for_raw_path(raw, &candidates), None);
    }

    #[test]
    fn malformed_paths_yield_none() {
        let candidates = vec![("unifying".to_string(), UNIFYING_CHAN.to_string())];
        assert_eq!(channel_for_raw_path("", &candidates), None);
        assert_eq!(channel_for_raw_path("no-hash-at-all", &candidates), None);
        assert_eq!(
            channel_for_raw_path(r"\\?\HID#NO_VID_HERE#7&1&0#{g}", &candidates),
            None
        );
        // 候选路径不可解析 → 不参与匹配。
        let bad = vec![("x".to_string(), "garbage".to_string())];
        assert_eq!(
            channel_for_raw_path(
                r"\\?\HID#VID_046D&PID_C52B&MI_01#7&2f3a1b2c&0&0001#{g}",
                &bad
            ),
            None
        );
    }

    #[test]
    fn fingerprint_is_case_insensitive() {
        let lower = r"\\?\hid#vid_046d&pid_c52b&mi_01#7&2f3a1b2c&0&0001#{g}";
        let candidates = vec![("unifying".to_string(), UNIFYING_CHAN.to_string())];
        assert_eq!(channel_for_raw_path(lower, &candidates), Some(0));
    }

    #[test]
    fn classified_hit_matches_option_version() {
        // BF-3：Hit 分支与原函数 Some 分支逐一一致。
        let raw = r"\\?\HID#VID_046D&PID_C52B&MI_01&Col02#7&2f3a1b2c&0&0001#{378de44c-56ef-11d1-bc8c-00a0c91405dd}";
        let candidates = vec![
            ("bolt".to_string(), BOLT_CHAN.to_string()),
            ("unifying".to_string(), UNIFYING_CHAN.to_string()),
        ];
        assert_eq!(
            channel_for_raw_path_classified(raw, &candidates),
            ChannelMatch::Hit(1)
        );
        assert_eq!(channel_for_raw_path(raw, &candidates), Some(1));
    }

    #[test]
    fn classified_splits_zero_match_from_ambiguous() {
        // BF-3 核心诉求：零匹配与歧义不再混为一个 None。
        // 零匹配（E22 后口径）：VID/PID 也不匹配的设备路径。
        let zero = r"\\?\HID#VID_046D&PID_9999&MI_01#7&DEADBEEF&0&0001#{378de44c-56ef-11d1-bc8c-00a0c91405dd}";
        let one = vec![("unifying".to_string(), UNIFYING_CHAN.to_string())];
        assert_eq!(
            channel_for_raw_path_classified(zero, &one),
            ChannelMatch::ZeroMatch
        );
        // 歧义：两个候选同指纹。
        let dup = vec![
            ("a".to_string(), UNIFYING_CHAN.to_string()),
            ("b".to_string(), UNIFYING_CHAN.to_string()),
        ];
        let raw = r"\\?\HID#VID_046D&PID_C52B&MI_01#7&2f3a1b2c&0&0001#{378de44c-56ef-11d1-bc8c-00a0c91405dd}";
        assert_eq!(
            channel_for_raw_path_classified(raw, &dup),
            ChannelMatch::Ambiguous
        );
        // 两者在原函数下均为 None（行为不变，仅遥测可区分）。
        assert_eq!(channel_for_raw_path(zero, &one), None);
        assert_eq!(channel_for_raw_path(raw, &dup), None);
    }

    #[test]
    fn classified_marks_unparseable_raw_path() {
        // BTHHID / 空串 / 无 VID 段 → NoFingerprint（系统性失配场景）。
        let candidates = vec![("unifying".to_string(), UNIFYING_CHAN.to_string())];
        let bt = r"\\?\BTHHID#{00001124-0000-1000-8000-00805f9b34fb}_VID&0002046d_PID_b01a#9&1c2d3e4f&0&0000#{4d1e55b2-f16e-11cf-88cb-001111000030}";
        assert_eq!(
            channel_for_raw_path_classified(bt, &candidates),
            ChannelMatch::NoFingerprint
        );
        assert_eq!(
            channel_for_raw_path_classified("", &candidates),
            ChannelMatch::NoFingerprint
        );
    }

    // -----------------------------------------------------------------
    // FR-1（RB2）验证：四场景 + E 双独立复核加测两场景
    // -----------------------------------------------------------------

    #[test]
    fn fr1_active_mapping_takes_priority() {
        // ① 正常在线场景：桶键取自活映射（与 Read 事件同命名空间）。
        let (k, src) = activity_bucket_key(Some("UID-A"), Some("UID-X"), r"\\?\hid#dev", true);
        assert_eq!(k, "UID-A");
        assert_eq!(src, ActivityKeySource::Active);
        // ③ 双命中不一致（持久=X、活=Y）→ 以活映射 Y 为准（修-1）。
        let (k2, _) = activity_bucket_key(Some("UID-Y"), Some("UID-X"), r"\\?\hid#dev", true);
        assert_eq!(k2, "UID-Y");
    }

    #[test]
    fn fr1_persistent_mapping_fallback_on_empty_slots() {
        // ② 重启后枚举未完成（空槽位窗口）：活映射 miss → 持久映射兜底。
        let (k, src) = activity_bucket_key(None, Some("UID-P"), r"\\?\hid#dev", true);
        assert_eq!(k, "UID-P");
        assert_eq!(src, ActivityKeySource::Persistent);
        // 活映射 miss 且通道也未归因命中 → 仍兜底持久映射。
        let (k2, src2) = activity_bucket_key(None, Some("UID-P"), r"\\?\hid#dev", false);
        assert_eq!(k2, "UID-P");
        assert_eq!(src2, ActivityKeySource::Persistent);
    }

    #[test]
    fn fr1_dual_miss_falls_back_to_raw_path() {
        // 双 miss：落原路径键（归因路径计 channel_map_miss，桶键侧不重复计）。
        let (k, src) = activity_bucket_key(None, None, r"\\?\hid#dev", false);
        assert_eq!(k, r"\\?\hid#dev");
        assert_eq!(src, ActivityKeySource::RawPathAttributionMiss);
        // 归因命中通道但槽位无 unitId：桶键侧补计 miss 的细分来源。
        let (k2, src2) = activity_bucket_key(None, None, r"\\?\hid#dev", true);
        assert_eq!(k2, r"\\?\hid#dev");
        assert_eq!(src2, ActivityKeySource::RawPathChannelKeyUnresolved);
        // 空 unitId 视同 miss。
        let (k3, src3) = activity_bucket_key(Some(""), Some(""), r"\\?\hid#dev", false);
        assert_eq!(k3, r"\\?\hid#dev");
        assert_eq!(src3, ActivityKeySource::RawPathAttributionMiss);
    }

    #[test]
    fn fr1_receiver_two_mice_distinguishable() {
        // ④ 接收器两鼠标场景：同一接收器两条 raw path 各自解析到
        // 不同 unitId 桶键——热力带不混串。
        let (k1, s1) =
            activity_bucket_key(Some("UID-M1"), None, r"\\?\hid#vid_046d#p#c532#m1", true);
        let (k2, s2) =
            activity_bucket_key(Some("UID-M2"), None, r"\\?\hid#vid_046d#p#c532#m2", true);
        assert_eq!((k1.as_str(), s1), ("UID-M1", ActivityKeySource::Active));
        assert_eq!((k2.as_str(), s2), ("UID-M2", ActivityKeySource::Active));
        assert_ne!(k1, k2);
        // 重启空槽位窗口：持久映射同样可区分两鼠。
        let (p1, _) =
            activity_bucket_key(None, Some("UID-M1"), r"\\?\hid#vid_046d#p#c532#m1", false);
        let (p2, _) =
            activity_bucket_key(None, Some("UID-M2"), r"\\?\hid#vid_046d#p#c532#m2", false);
        assert_ne!(p1, p2);
    }

    // -----------------------------------------------------------------
    // E10 两级匹配（N-01/N-02）fixture 回放。注：refs/ 取证原件无真机
    // 路径样本（39 条报告为 JSON 存根），fixture 由既有真机形态常量 +
    // 实例漂移场景构造——真机回放属未验证边界，见交付说明。
    // -----------------------------------------------------------------

    #[test]
    fn e10_exact_hit_preferred_over_relaxed() {
        // 精确指纹唯一命中 → HitExact（两级中精确优先）。
        let raw = r"\\?\HID#VID_046D&PID_C52B&MI_01&Col02#7&2f3a1b2c&0&0001#{378de44c-56ef-11d1-bc8c-00a0c91405dd}";
        let candidates = vec![
            ("bolt".to_string(), BOLT_CHAN.to_string()),
            ("unifying".to_string(), UNIFYING_CHAN.to_string()),
        ];
        assert_eq!(
            channel_match_gated(raw, &candidates, &[]),
            TwoLevelMatch::HitExact(1)
        );
    }

    #[test]
    fn e10_relaxed_hit_when_instance_drifts() {
        // 换 USB 口漂移：实例段完全不同，VID/PID 级唯一 → HitRelaxed
        // （E22 单候选回退在多候选下的推广形态）。
        let raw = r"\\?\HID#VID_046D&PID_C52B&MI_01#9&EEEE0001&0&0001#{378de44c-56ef-11d1-bc8c-00a0c91405dd}";
        let candidates = vec![
            ("bolt".to_string(), BOLT_CHAN.to_string()),
            ("unifying".to_string(), UNIFYING_CHAN.to_string()),
        ];
        assert_eq!(
            channel_match_gated(raw, &candidates, &[]),
            TwoLevelMatch::HitRelaxed(1)
        );
    }

    #[test]
    fn e10_relaxed_zero_match_when_vid_pid_absent() {
        let raw = r"\\?\HID#VID_046D&PID_9999&MI_01#7&DEADBEEF&0&0001#{g}";
        let candidates = vec![
            ("bolt".to_string(), BOLT_CHAN.to_string()),
            ("unifying".to_string(), UNIFYING_CHAN.to_string()),
        ];
        assert_eq!(
            channel_match_gated(raw, &candidates, &[]),
            TwoLevelMatch::ZeroMatch
        );
    }

    #[test]
    fn e10_mi_mismatch_excludes_candidate() {
        // 双方都有 MI 且不同 → 不兼容：MI_02 候选被排除，无 MI 的
        // 接收器通道路径兼容命中。
        let chan_mi02 = r"\\?\HID#VID_046D&PID_C52B&MI_02#7&2f3a1b2c&0&0000#{g}";
        let raw = r"\\?\HID#VID_046D&PID_C52B&MI_01#9&EEEE0001&0&0001#{g}";
        let candidates = vec![
            ("mi02".to_string(), chan_mi02.to_string()),
            ("unifying".to_string(), UNIFYING_CHAN.to_string()),
        ];
        assert_eq!(
            channel_match_gated(raw, &candidates, &[]),
            TwoLevelMatch::HitRelaxed(1)
        );
    }

    #[test]
    fn e10_binding_table_disambiguates_same_model_receivers() {
        // 同型号两台接收器 + 换口重枚举：通道路径实例段已变，Raw
        // Input 路径仍是注册期旧实例前缀——精确级全 miss、放宽级
        // 双候选；绑定表（注册期观察到的设备实例段）最长公共前缀
        // 消歧命中，无绑定表则歧义走 arm-all。
        let chan_a = r"\\?\HID#VID_046D&PID_C52B#8&aaaa0001&0&0000#{g}";
        let chan_b = r"\\?\HID#VID_046D&PID_C52B#9&bbbb0002&0&0000#{g}";
        let raw = r"\\?\HID#VID_046D&PID_C52B&MI_01#7&2f3a1b2c&0&0001#{g}";
        let candidates = vec![
            ("a".to_string(), chan_a.to_string()),
            ("b".to_string(), chan_b.to_string()),
        ];
        assert_eq!(
            channel_match_gated(raw, &candidates, &[]),
            TwoLevelMatch::Ambiguous
        );
        let bindings = vec![
            WakeBinding {
                channel_key: "a".to_string(),
                unit_id: Some("UID-MOUSE-A".to_string()),
                instance_parts: vec![
                    "7".into(),
                    "2F3A1B2C".into(),
                    "0".into(),
                    "0001".into(),
                ],
            },
            WakeBinding {
                channel_key: "b".to_string(),
                unit_id: Some("UID-MOUSE-B".to_string()),
                instance_parts: vec![
                    "6".into(),
                    "9999CCCC".into(),
                    "0".into(),
                    "0001".into(),
                ],
            },
        ];
        assert_eq!(
            channel_match_gated(raw, &candidates, &bindings),
            TwoLevelMatch::HitRelaxed(0)
        );
    }

    #[test]
    fn e10_ambiguous_when_disambiguation_ties() {
        // 双候选与 raw 公共前缀等长（全 0）→ 歧义即 miss。
        let chan_a = r"\\?\HID#VID_046D&PID_C52B#7&2f3a1b2c&0&0000#{g}";
        let chan_b = r"\\?\HID#VID_046D&PID_C52B#8&4444aaaa&0&0000#{g}";
        let raw = r"\\?\HID#VID_046D&PID_C52B&MI_01#9&eeee0001&0&0001#{g}";
        let candidates = vec![
            ("a".to_string(), chan_a.to_string()),
            ("b".to_string(), chan_b.to_string()),
        ];
        assert_eq!(
            channel_match_gated(raw, &candidates, &[]),
            TwoLevelMatch::Ambiguous
        );
    }

    #[test]
    fn e10_no_fingerprint_passthrough() {
        let candidates = vec![("unifying".to_string(), UNIFYING_CHAN.to_string())];
        let bt = r"\\?\BTHHID#{00001124-0000-1000-8000-00805f9b34fb}_VID&0002046d_PID_b01a#9&1c2d3e4f&0&0000#{g}";
        assert_eq!(
            channel_match_gated(bt, &candidates, &[]),
            TwoLevelMatch::NoFingerprint
        );
        assert_eq!(
            channel_match_gated("", &candidates, &[]),
            TwoLevelMatch::NoFingerprint
        );
    }
}
