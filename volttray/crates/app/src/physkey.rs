//! 物理身份（T6）的纯逻辑决策：别名晋级挑选与 GUI pin 重绑匹配。
//!
//! 两条规则都不发起任何设备通信，只在主机侧既有状态上判定：
//! - [`pick_promotion`]：主端点所在通道被移除（拔线）时，从仍存活的通道里
//!   挑一个同 unitId 的别名槽位晋级为新主端点——无线回落后身份连续
//!   （电量 / 低电追踪 / pin 选定不丢）。
//! - [`repin`]：rebind 改变条目 id 后，GUI 侧把用户 pin 选定的旧 id 经
//!   物理身份重匹配到新 id。
//!
//! 保守规则与 core 一致：只认双方 proven unitId；身份不明（None）不晋级、
//! 不重绑。

use crate::core::Snapshot;

/// 晋级判定用的槽位视图（由 IO 线程从 SlotState 投影而来）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasSlot {
    /// 槽位所属通道本轮枚举仍存活（`seen`）。
    pub channel_alive: bool,
    /// 端点 id（`{pid}|{tag}#{slot}`）。
    pub id: String,
    /// 是否为别名槽位（同物理设备的重复端点，未参与轮询）。
    pub alias: bool,
    /// 注册期读到的 unitId（None = 身份不明）。
    pub unit_id: Option<String>,
}

/// 拔线晋级：被移除主端点的 unitId → 仍存活通道里同 unitId 的别名槽位下标。
///
/// 只挑 `alias && channel_alive && unit_id == Some(removed_unit_id)` 的槽位；
/// 找不到（无线别名也随枚举消失等）返回 None，调用方按现状移除条目。
pub fn pick_promotion(removed_unit_id: &str, slots: &[AliasSlot]) -> Option<usize> {
    slots
        .iter()
        .position(|s| s.alias && s.channel_alive && s.unit_id.as_deref() == Some(removed_unit_id))
}

/// 晋级验明（红蓝对抗 P1-1 必修）：候选别名端点的即时重读 unitId 与
/// 被移除主端点的冻结 unitId 一致才允许晋级。
///
/// 背景：槽位的 unit_id 是注册期一次性写入的冻结值；接收器槽位在别名
/// 存续期间可能被 RF 侧重配对（Logi 配对软件，纯 RF 操作、USB 枚举签名
/// 不变、不触发重扫），此后冻结值不再代表端点当前身份。直接凭冻结值
/// 晋级会把台账重绑到另一台物理鼠标的端点（误并），并永久压制该鼠标。
/// 因此晋级前必须一次性重读验明：重读失败（None）或值不同都拒绝晋级。
pub fn identity_confirmed(frozen: &str, reread: Option<&str>) -> bool {
    reread == Some(frozen)
}

/// pin 重绑匹配：快照更新后，若用户 pin 的 id 已不在新快照里，用旧快照中
/// 该设备的 proven 物理身份在新快照里找同一物理设备，返回应改 pin 的新 id。
///
/// 找不到匹配（设备真消失 / 身份不明）返回 None——调用方保留旧 pin，
/// 由既有「pin 设备不在快照 → 回退默认」逻辑处理。
pub fn repin(pinned: &Option<String>, old: &Snapshot, new: &Snapshot) -> Option<String> {
    let pinned = pinned.as_deref()?;
    if new.devices.iter().any(|d| d.id == pinned) {
        return None; // pin 仍有效，无需重绑
    }
    let key = old
        .devices
        .iter()
        .find(|d| d.id == pinned)
        .and_then(|d| d.physical_key.as_deref())?;
    new.devices
        .iter()
        .find(|d| d.physical_key.as_deref() == Some(key))
        .map(|d| d.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::DeviceSnapshot;

    fn slot(alive: bool, id: &str, alias: bool, unit: Option<&str>) -> AliasSlot {
        AliasSlot {
            channel_alive: alive,
            id: id.into(),
            alias,
            unit_id: unit.map(str::to_string),
        }
    }

    fn snap_row(id: &str, key: Option<&str>) -> DeviceSnapshot {
        DeviceSnapshot {
            id: id.into(),
            name: format!("Mouse {id}"),
            percent: Some(50),
            charging: false,
            link: "bolt".into(),
            state: "online".into(),
            gray: false,
            source: None,
            physical_key: key.map(str::to_string),
            is_virtual: false,
            last_seen: None,
            reading_age_secs: None,
        }
    }

    fn snapshot(rows: Vec<DeviceSnapshot>) -> Snapshot {
        Snapshot {
            seq: 1,
            devices: rows,
            default_device: None,
        }
    }

    #[test]
    fn promotion_picks_alive_alias_with_same_unit_id() {
        let slots = vec![
            slot(false, "wired#255", false, Some("AAAA0001")), // 被移除的主端点
            slot(true, "recv#1", true, Some("AAAA0001")),      // 存活别名 → 晋级
            slot(true, "other#2", true, Some("BBBB0002")),     // 别的鼠标
        ];
        assert_eq!(pick_promotion("AAAA0001", &slots), Some(1));
    }

    #[test]
    fn promotion_rejects_dead_channels_non_alias_and_unknown_identity() {
        // 别名所在通道也已消失（有线接口随拔线移除）→ 不晋级。
        let slots = vec![slot(false, "recv#1", true, Some("AAAA0001"))];
        assert_eq!(pick_promotion("AAAA0001", &slots), None);
        // 非别名槽位不参与晋级（它是另一台设备或主端点自身）。
        let slots = vec![slot(true, "recv#1", false, Some("AAAA0001"))];
        assert_eq!(pick_promotion("AAAA0001", &slots), None);
        // 身份不明的别名不晋级（保守规则）。
        let slots = vec![slot(true, "recv#1", true, None)];
        assert_eq!(pick_promotion("AAAA0001", &slots), None);
        // 无同 unitId 别名。
        let slots = vec![slot(true, "recv#1", true, Some("BBBB0002"))];
        assert_eq!(pick_promotion("AAAA0001", &slots), None);
    }

    #[test]
    fn identity_confirmed_requires_matching_reread() {
        // 重读一致 → 放行晋级。
        assert!(identity_confirmed("AAAA0001", Some("AAAA0001")));
        // 重读失败（设备睡了 / 超时 / 协议错）→ 拒绝（保守：不误并）。
        assert!(!identity_confirmed("AAAA0001", None));
        // 端点已被重配对给另一台鼠标 → 拒绝（P1-1 攻击路径）。
        assert!(!identity_confirmed("AAAA0001", Some("BBBB0002")));
    }

    #[test]
    fn repin_follows_physical_key_across_rebind() {
        let old = snapshot(vec![snap_row("recv#1", Some("AAAA0001"))]);
        let new = snapshot(vec![snap_row("wired&MI_02#255", Some("AAAA0001"))]);
        let pinned = Some("recv#1".to_string());
        assert_eq!(
            repin(&pinned, &old, &new).as_deref(),
            Some("wired&MI_02#255")
        );
    }

    #[test]
    fn repin_keeps_pin_when_id_still_present_or_identity_unknown() {
        // pin 的 id 仍在新快照 → None（不动）。
        let old = snapshot(vec![snap_row("a", Some("AAAA0001"))]);
        let new = snapshot(vec![snap_row("a", Some("AAAA0001"))]);
        assert_eq!(repin(&Some("a".into()), &old, &new), None);
        // 旧快照里该设备身份不明 → 不重绑（保守）。
        let old = snapshot(vec![snap_row("a", None)]);
        let new = snapshot(vec![snap_row("b", Some("AAAA0001"))]);
        assert_eq!(repin(&Some("a".into()), &old, &new), None);
        // 新快照里没有同物理身份设备（真消失）→ None，走既有回退。
        let old = snapshot(vec![snap_row("a", Some("AAAA0001"))]);
        let new = snapshot(vec![snap_row("b", Some("BBBB0002"))]);
        assert_eq!(repin(&Some("a".into()), &old, &new), None);
        // 无 pin。
        assert_eq!(repin(&None, &old, &new), None);
    }
}
