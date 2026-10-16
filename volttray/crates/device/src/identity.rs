//! 物理设备身份读取（T6 有线模式重复条目去重）。
//!
//! 来源：HID++ DEVICE_FW_VERSION(0x0003) fn0 GetEntityFwVersion(entity=0) 应答
//! params[1:5] = unitId（Solaar hidpp20.get_ids 实证字节布局；modelId 在
//! params[7:13]，本模块不取）。unitId 是物理设备级身份——同一鼠标经接收器
//! 任意槽位或有线任意 USB 接口读到的值一致，因此可用于跨端点去重。
//!
//! 电池红线：本函数只允许在既有注册/握手流程中调用一次（设备刚应答过
//! 握手与 feature 探测，必然醒着），不进入任何周期轮询路径；不为去重
//! 单独发起额外设备通信之外的任何周期性查询。
//!
//! 失败一律返回 None（feature 未解析 / 超时 / 协议错误 / 全 0 / 全 FF），
//! 调用方走「身份不明不合并」降级路径（保守规则：绝不误并两台独立鼠标）。

use volttray_protocol::commands::{feature_ids, QueryCommand};
use volttray_protocol::FeatureTable;
use volttray_transport::channel::HidChannel;
use volttray_transport::events::EventSink;
use volttray_transport::Transport;

/// 读取物理设备 unitId（大写 hex，8 字符）；取不到返回 None。
pub fn read_unit_id<C: HidChannel, S: EventSink>(
    t: &mut Transport<C, S>,
    device_index: u8,
    features: &FeatureTable,
) -> Option<String> {
    features.get(feature_ids::DEVICE_FW_VERSION)?;
    let sw = t.alloc_sw_id();
    let req = QueryCommand::GetUnitId
        .encode_for(device_index, sw, features)
        .ok()?;
    let frame = t.request(&req, Some(feature_ids::DEVICE_FW_VERSION)).ok()?;
    parse_unit_id(frame.params())
}

/// 从 fn0 应答参数区提取 unitId：params[1:5]，全 0x00 / 全 0xFF 视为无身份。
fn parse_unit_id(params: &[u8]) -> Option<String> {
    if params.len() < 5 {
        return None;
    }
    let id = &params[1..5];
    if id.iter().all(|&b| b == 0x00) || id.iter().all(|&b| b == 0xFF) {
        return None;
    }
    Some(id.iter().map(|b| format!("{b:02X}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::time::Duration;
    use volttray_transport::channel::IoError;
    use volttray_transport::events::DiagEvent;

    struct Stub {
        reads: VecDeque<Result<Vec<u8>, IoError>>,
    }
    impl HidChannel for Stub {
        fn write(&mut self, d: &[u8]) -> Result<usize, IoError> {
            Ok(d.len())
        }
        fn read(&mut self, buf: &mut [u8], _t: Duration) -> Result<usize, IoError> {
            match self.reads.pop_front().unwrap_or(Err(IoError::Timeout)) {
                Ok(data) => {
                    buf[..data.len()].copy_from_slice(&data);
                    Ok(data.len())
                }
                Err(e) => Err(e),
            }
        }
    }
    struct Sink;
    impl EventSink for Sink {
        fn emit(&mut self, _e: DiagEvent) {}
    }

    fn stub(reads: Vec<Result<Vec<u8>, IoError>>) -> Transport<Stub, Sink> {
        Transport::new(
            Stub {
                reads: reads.into(),
            },
            Sink,
        )
    }

    fn feature_table() -> FeatureTable {
        let mut t = FeatureTable::new();
        t.insert(feature_ids::DEVICE_FW_VERSION, 0x02);
        t
    }

    /// fn0 应答：byte3 = 0x01（fn0 | 首个 sw_id=1，与全新 Transport 的
    /// alloc_sw_id 序列对齐），unitId = params[1:5] = [0x39, 0x41, 0x42, 0x43]。
    fn unit_id_frame() -> Vec<u8> {
        let mut f = vec![0x11, 0x01, 0x02, 0x01, 0x00, 0x39, 0x41, 0x42, 0x43];
        f.extend(std::iter::repeat_n(0u8, 20 - f.len()));
        f
    }

    #[test]
    fn reads_unit_id_from_fn0_params() {
        let mut t = stub(vec![Ok(unit_id_frame())]);
        let id = read_unit_id(&mut t, 0x01, &feature_table());
        assert_eq!(id.as_deref(), Some("39414243"));
    }

    #[test]
    fn all_zero_or_all_ff_unit_id_rejected() {
        let mut zero = vec![0x11, 0x01, 0x02, 0x01];
        zero.extend(std::iter::repeat_n(0u8, 16));
        let mut t = stub(vec![Ok(zero)]);
        assert_eq!(read_unit_id(&mut t, 0x01, &feature_table()), None);

        let mut ff = vec![0x11, 0x01, 0x02, 0x01, 0x00];
        ff.extend(std::iter::repeat_n(0xFFu8, 15));
        let mut t2 = stub(vec![Ok(ff)]);
        assert_eq!(read_unit_id(&mut t2, 0x01, &feature_table()), None);
    }

    #[test]
    fn unresolved_feature_or_timeout_returns_none() {
        // feature 未解析 → 不发请求直接 None（无通信，红线内）。
        let mut t = stub(vec![]);
        assert_eq!(read_unit_id(&mut t, 0x01, &FeatureTable::new()), None);
        // 超时 → None（降级路径）。
        let mut t2 = stub(vec![Err(IoError::Timeout)]);
        assert_eq!(read_unit_id(&mut t2, 0x01, &feature_table()), None);
    }

    #[test]
    fn parse_unit_id_unit() {
        assert_eq!(
            parse_unit_id(&[0x00, 0x11, 0x22, 0x33, 0x44, 0x99]),
            Some("11223344".to_string())
        );
        assert_eq!(parse_unit_id(&[0x00, 0, 0, 0, 0]), None);
        assert_eq!(parse_unit_id(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF]), None);
        assert_eq!(parse_unit_id(&[0x00, 0x11, 0x22]), None); // 短帧
    }
}
