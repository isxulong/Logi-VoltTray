//! 重枚举与重连裁决（v0.3.9，D-65）：无设备态退避重试、「立即刷新」强制
//! 重枚举、句柄失效自愈的纯逻辑唯一裁决点。
//!
//! 背景（真机报告：系统重启后托盘常驻「?」且手动刷新无效）：
//! - v0.3.8 的 IO 循环在开机自启早于 USB 接收器/驱动就绪时枚举到 0 个设备，
//!   随后按轮询间隔（默认 600s）睡眠——「无设备」态下一次自动重试最晚
//!   10 分钟后才发生（启动时序竞争，怀疑方向 1）；
//! - 「立即刷新」仅 `continue` 回到循环头，重枚举受 30s 门控/通道非空跳过，
//!   通道存在但槽位扫描落空时刷新只重复查询缓存句柄（怀疑方向 3）；
//! - HID 句柄打开后缓存复用，驱动重载/接收器重枚举后句柄失效，所有查询
//!   打到死句柄上，3 连败只移除槽位、死通道永驻，重扫描永远落空（怀疑方向 2）。
//!
//! 零鼠标电池约束：本模块裁决的「枚举」全部是主机侧 HID API 操作
//! （`HidApi::new` + 设备列表遍历），不产生任何射频流量；无设备态退避
//! 重试只放松枚举节奏（30s → 60s → 120s 封顶），**不提高**任何查询频率；
//! 对已注册的休眠设备不新增任何主动探测（D-33 休眠 Skip 语义不变）。

use std::time::{Duration, Instant};

/// 有通道（或正常态）时的例行重枚举间隔（沿用 v0.2.0 以来的 30s 门控）。
pub const REENUM_INTERVAL: Duration = Duration::from_secs(30);

/// 无设备态（枚举不到任何 HID++ 候选通道）的退避重试档位：30s → 60s → 120s 封顶。
/// 仅主机侧枚举、不向设备发任何查询，不违反零耗电约束。
pub const ENUM_BACKOFF_STEPS: [Duration; 3] = [
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
];

/// 连续非超时类硬失败（I/O 错误等）达到该次数即判定通道句柄失效：
/// 作废缓存句柄并强制重新枚举一次（主机侧操作，零射频成本）。
pub const HANDLE_INVALIDATE_THRESHOLD: u32 = 3;

/// 枚举结果的态迁移（供 IO 线程写诊断日志/事件）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnumTransition {
    /// 本次枚举后进入「无设备」态（此前有设备）。
    EnteredNoDevice,
    /// 本次枚举后退出「无设备」态（重新枚举到设备，恢复正常轮询）。
    ExitedNoDevice,
}

/// 重枚举/重连状态机（IO 线程持有，Instant 注入可测）。
#[derive(Debug, Clone)]
pub struct Reconnect {
    /// 连续「枚举后注册槽位总数为 0」的次数（决定退避档位）。
    empty_streak: u32,
    /// 上次枚举完成的时刻（None = 尚未枚举过，首轮立即枚举）。
    last_enum: Option<Instant>,
    /// 是否处于「无设备」态。
    no_device: bool,
    /// 强制下一轮循环立即重枚举（「立即刷新」/句柄失效自愈置位）。
    force: bool,
}

impl Default for Reconnect {
    fn default() -> Self {
        Self::new()
    }
}

impl Reconnect {
    pub fn new() -> Self {
        Reconnect {
            empty_streak: 0,
            last_enum: None,
            no_device: false,
            force: false,
        }
    }

    /// 「立即刷新」：下一轮循环必须先重新枚举再查询（怀疑方向 3 修复）。
    pub fn request_refresh(&mut self) {
        self.force = true;
    }

    /// 缓存句柄失效（连续 I/O 硬失败达阈值）：作废句柄并在下一轮
    /// 立即重新枚举（重开句柄、重扫槽位；主机侧操作，零射频成本）。
    pub fn invalidate_handles(&mut self) {
        self.force = true;
    }

    /// 是否有待消费的强制重枚举（IO 线程据此跳过本轮睡眠）。
    pub fn pending_force(&self) -> bool {
        self.force
    }

    pub fn no_device(&self) -> bool {
        self.no_device
    }

    /// 当前退避档位（无设备且无任何候选通道时的重试间隔）。
    pub fn current_backoff(&self) -> Duration {
        let idx = self
            .empty_streak
            .max(1)
            .min(ENUM_BACKOFF_STEPS.len() as u32)
            - 1;
        ENUM_BACKOFF_STEPS[idx as usize]
    }

    /// 本轮循环是否执行重枚举。
    ///
    /// - 有待消费的强制请求 → 枚举；
    /// - 从未枚举过 → 枚举；
    /// - 无设备且连候选通道都没有 → 按 30s/60s/120s 退避（仅主机侧枚举）；
    /// - 其余（正常态 / 有通道但无注册槽位）→ 沿用 30s 门控。
    pub fn should_enumerate(&self, now: Instant, has_channels: bool) -> bool {
        if self.force {
            return true;
        }
        let Some(t) = self.last_enum else {
            return true;
        };
        let elapsed = now.saturating_duration_since(t);
        if self.no_device && !has_channels {
            elapsed >= self.current_backoff()
        } else {
            elapsed >= REENUM_INTERVAL
        }
    }

    /// 登记一次枚举完成。`registered_slots` = 枚举+槽位扫描后全部通道的
    /// 注册槽位总数（用户可见的「有无设备」口径）。返回态迁移供打日志。
    pub fn record_result(
        &mut self,
        now: Instant,
        registered_slots: usize,
    ) -> Option<EnumTransition> {
        self.last_enum = Some(now);
        self.force = false;
        if registered_slots == 0 {
            self.empty_streak = self.empty_streak.saturating_add(1);
            if !self.no_device {
                self.no_device = true;
                return Some(EnumTransition::EnteredNoDevice);
            }
            None
        } else {
            self.empty_streak = 0;
            if self.no_device {
                self.no_device = false;
                return Some(EnumTransition::ExitedNoDevice);
            }
            None
        }
    }

    /// 本轮轮询后的睡眠时长（分片睡眠的总预算）：
    /// - 有待消费的强制重枚举 → 0（立即回到循环头执行枚举）；
    /// - 无设备态 → 距下次退避枚举的剩余时间（无候选通道时 30/60/120s，
    ///   有通道但无槽位时沿用 30s；不产生任何设备查询）；
    /// - 正常态 → 轮询间隔（与 v0.3.8 行为一致，重枚举在醒后随 30s 门控发生）。
    pub fn next_wake(&self, now: Instant, has_channels: bool, poll_interval: Duration) -> Duration {
        if self.force {
            return Duration::ZERO;
        }
        if !self.no_device {
            return poll_interval;
        }
        let due = if self.no_device && !has_channels {
            self.current_backoff()
        } else {
            REENUM_INTERVAL
        };
        match self.last_enum {
            Some(t) => due.saturating_sub(now.saturating_duration_since(t)),
            None => Duration::ZERO,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 枚举失败 → 进入无设备态 → 退避 30s/60s/120s 封顶 → 枚举恢复 →
    /// 退出无设备态并恢复正常轮询节奏（任务 B 用例 1）。
    #[test]
    fn enum_fail_no_device_backoff_then_recovery() {
        let mut r = Reconnect::new();
        let t0 = Instant::now();
        // 首轮（开机自启）立即枚举。
        assert!(r.should_enumerate(t0, false));
        // 枚举到 0 设备（接收器/驱动未就绪）→ 进入无设备态。
        assert_eq!(
            r.record_result(t0, 0),
            Some(EnumTransition::EnteredNoDevice)
        );
        assert!(r.no_device());
        assert_eq!(r.current_backoff(), Duration::from_secs(30));
        // 立即重试被抑制：30s 内不再枚举（不提高频率）。
        assert!(!r.should_enumerate(t0 + Duration::from_secs(29), false));
        assert!(r.should_enumerate(t0 + Duration::from_secs(30), false));
        // 第二次落空 → 退避升 60s。
        let t1 = t0 + Duration::from_secs(30);
        assert_eq!(r.record_result(t1, 0), None); // 已在无设备态，无重复迁移事件
        assert_eq!(r.current_backoff(), Duration::from_secs(60));
        assert!(!r.should_enumerate(t1 + Duration::from_secs(59), false));
        assert!(r.should_enumerate(t1 + Duration::from_secs(60), false));
        // 第三次落空 → 120s 封顶。
        let t2 = t1 + Duration::from_secs(60);
        assert_eq!(r.record_result(t2, 0), None);
        assert_eq!(r.current_backoff(), Duration::from_secs(120));
        // 继续落空保持 120s 封顶（不无限增长）。
        let t3 = t2 + Duration::from_secs(120);
        assert_eq!(r.record_result(t3, 0), None);
        assert_eq!(r.current_backoff(), Duration::from_secs(120));
        // 枚举恢复（驱动就绪，注册到槽位）→ 退出无设备态。
        let t4 = t3 + Duration::from_secs(120);
        assert_eq!(r.record_result(t4, 2), Some(EnumTransition::ExitedNoDevice));
        assert!(!r.no_device());
        assert_eq!(r.current_backoff(), Duration::from_secs(30)); // 连击清零
                                                                  // 恢复正常轮询：next_wake 回到轮询间隔，重枚举恢复 30s 门控。
        let poll = Duration::from_secs(600);
        assert_eq!(r.next_wake(t4, true, poll), poll);
        assert!(!r.should_enumerate(t4 + Duration::from_secs(29), true));
        assert!(r.should_enumerate(t4 + Duration::from_secs(30), true));
    }

    /// 「立即刷新」必须先重新枚举再查询（任务 B 用例 2 / 怀疑方向 3）。
    #[test]
    fn refresh_forces_reenumeration() {
        let mut r = Reconnect::new();
        let t0 = Instant::now();
        assert_eq!(r.record_result(t0, 1), None); // 正常态：刚枚举完、有设备
                                                  // 30s 门控内本不该重枚举。
        assert!(!r.should_enumerate(t0 + Duration::from_secs(5), true));
        // 用户点「立即刷新」→ 下一轮立即重枚举、睡眠预算归零。
        r.request_refresh();
        assert!(r.pending_force());
        assert!(r.should_enumerate(t0 + Duration::from_secs(5), true));
        assert_eq!(
            r.next_wake(t0 + Duration::from_secs(5), true, Duration::from_secs(600)),
            Duration::ZERO
        );
        // 枚举完成后强制位被消费，恢复正常门控。
        assert_eq!(r.record_result(t0 + Duration::from_secs(5), 1), None);
        assert!(!r.pending_force());
        assert!(!r.should_enumerate(t0 + Duration::from_secs(10), true));
    }

    /// 无设备态下「立即刷新」同样立即重枚举（不受退避抑制——用户显式
    /// 操作，枚举本身是主机侧零射频操作）。
    #[test]
    fn refresh_overrides_no_device_backoff() {
        let mut r = Reconnect::new();
        let t0 = Instant::now();
        assert_eq!(
            r.record_result(t0, 0),
            Some(EnumTransition::EnteredNoDevice)
        );
        assert!(!r.should_enumerate(t0 + Duration::from_secs(1), false));
        r.request_refresh();
        assert!(r.should_enumerate(t0 + Duration::from_secs(1), false));
        assert_eq!(
            r.next_wake(t0 + Duration::from_secs(1), false, Duration::from_secs(600)),
            Duration::ZERO
        );
    }

    /// 句柄失效自愈（任务 B 用例 3 / 怀疑方向 2）：连续 I/O 硬失败达阈值 →
    /// 作废缓存句柄、强制重枚举一次 → 重开句柄重扫槽位 → 恢复正常。
    #[test]
    fn handle_invalidation_self_heals() {
        let mut r = Reconnect::new();
        let t0 = Instant::now();
        assert_eq!(r.record_result(t0, 2), None); // 两台设备在线
                                                  // 驱动重载导致缓存句柄死亡：IO 线程在硬失败达阈值后调用
                                                  // invalidate_handles（并丢弃旧通道），下一轮立即重枚举。
        assert!(!r.should_enumerate(t0 + Duration::from_secs(3), true));
        r.invalidate_handles();
        assert!(r.should_enumerate(t0 + Duration::from_secs(3), true));
        assert_eq!(
            r.next_wake(t0 + Duration::from_secs(3), false, Duration::from_secs(600)),
            Duration::ZERO,
            "旧通道已丢弃、句柄待重开：不得睡眠等待"
        );
        // 重枚举重开句柄、重扫槽位成功 → 正常态，期间无多余的态迁移事件。
        assert_eq!(r.record_result(t0 + Duration::from_secs(4), 2), None);
        assert!(!r.no_device());
        assert!(!r.pending_force());
        assert_eq!(
            r.next_wake(t0 + Duration::from_secs(4), true, Duration::from_secs(600)),
            Duration::from_secs(600)
        );
    }

    /// 硬失败达阈值常量：与 IO 线程的 3 连败注销口径一致。
    #[test]
    fn invalidate_threshold_is_three() {
        assert_eq!(HANDLE_INVALIDATE_THRESHOLD, 3);
    }

    /// 接收器通道在、但槽位扫描全落空（驱动未就绪的中间态）：
    /// 无设备态下沿用 30s 重扫节奏（既有行为），不走 60/120 退避。
    #[test]
    fn no_device_with_channels_keeps_30s_rescan() {
        let mut r = Reconnect::new();
        let t0 = Instant::now();
        assert_eq!(
            r.record_result(t0, 0),
            Some(EnumTransition::EnteredNoDevice)
        );
        assert_eq!(r.record_result(t0 + Duration::from_secs(30), 0), None);
        // 有通道：30s 门控，不升退避档。
        assert!(!r.should_enumerate(t0 + Duration::from_secs(59), true));
        assert!(r.should_enumerate(t0 + Duration::from_secs(60), true));
        // next_wake 给 30s 剩余，而非 60s 退避。
        assert_eq!(
            r.next_wake(t0 + Duration::from_secs(30), true, Duration::from_secs(600)),
            Duration::from_secs(30)
        );
    }

    /// 无设备态的睡眠预算 = 距下次退避枚举的剩余时间（主机侧等待，
    /// 期间零查询）；超过档位立即醒来枚举。
    #[test]
    fn next_wake_tracks_backoff_remaining() {
        let mut r = Reconnect::new();
        let t0 = Instant::now();
        assert_eq!(
            r.record_result(t0, 0),
            Some(EnumTransition::EnteredNoDevice)
        );
        assert_eq!(
            r.next_wake(
                t0 + Duration::from_secs(10),
                false,
                Duration::from_secs(600)
            ),
            Duration::from_secs(20)
        );
        assert_eq!(
            r.next_wake(
                t0 + Duration::from_secs(30),
                false,
                Duration::from_secs(600)
            ),
            Duration::ZERO
        );
    }
}
