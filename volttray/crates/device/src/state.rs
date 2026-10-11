//! 设备生命周期状态机（蓝图 5.6：显式状态机，非法迁移 panic）。
//!
//! `GrayedOut` 不在迁移表内：它是 app 依据“距上次成功 90s”**派生**出的展示态，
//! 不作为持久状态迁移目标（蓝图 5.6 + PRD M2）。

/// 设备生命周期状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceState {
    /// 已在总线上发现，尚未完成协议握手。
    Connected,
    /// 协议握手完成，可正常查询。
    Online,
    /// 设备休眠（查询超时/设备忙），保留会话。
    Sleeping,
    /// 设备断开（I/O 报 DeviceGone）。
    Disconnected,
    /// 派生展示态：90s 无成功响应（不进入迁移表）。
    GrayedOut,
}

impl DeviceState {
    pub fn name(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Online => "online",
            Self::Sleeping => "sleeping",
            Self::Disconnected => "disconnected",
            Self::GrayedOut => "grayed_out",
        }
    }
}

/// 合法迁移表（穷尽枚举，无通配）。
pub fn is_legal_transition(from: DeviceState, to: DeviceState) -> bool {
    use DeviceState::*;
    matches!(
        (from, to),
        (Connected, Online)
            | (Connected, Disconnected)
            | (Online, Sleeping)
            | (Online, Disconnected)
            | (Sleeping, Online)
            | (Sleeping, Disconnected)
            | (Disconnected, Connected)
    )
}

/// 执行迁移；非法迁移 panic（蓝图裁决：状态机错误是编程错误，应尽早暴露）。
pub fn transition(from: DeviceState, to: DeviceState) -> DeviceState {
    assert!(
        is_legal_transition(from, to),
        "illegal device state transition: {} -> {}",
        from.name(),
        to.name()
    );
    to
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_lifecycle_sequence() {
        let s = DeviceState::Connected;
        let s = transition(s, DeviceState::Online);
        let s = transition(s, DeviceState::Sleeping);
        let s = transition(s, DeviceState::Online);
        let s = transition(s, DeviceState::Disconnected);
        let s = transition(s, DeviceState::Connected);
        let s = transition(s, DeviceState::Online);
        assert_eq!(s, DeviceState::Online);
    }

    #[test]
    #[should_panic(expected = "illegal device state transition")]
    fn illegal_transition_panics() {
        let _ = transition(DeviceState::Connected, DeviceState::Sleeping);
    }

    #[test]
    #[should_panic(expected = "illegal device state transition")]
    fn grayed_out_is_not_a_transition_target() {
        let _ = transition(DeviceState::Online, DeviceState::GrayedOut);
    }

    #[test]
    fn grayed_out_has_no_outgoing_edges() {
        use DeviceState::*;
        for to in [Connected, Online, Sleeping, Disconnected, GrayedOut] {
            assert!(!is_legal_transition(GrayedOut, to));
        }
    }
}
