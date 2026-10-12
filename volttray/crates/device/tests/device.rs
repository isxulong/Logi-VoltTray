//! 设备层集成测试：策略链（桩通道）+ 映射函数。

use std::collections::VecDeque;
use std::time::Duration;
use volttray_device::battery::{
    map_battery_status, map_battery_voltage, map_hidpp1_register_0x0d, map_unified_battery,
    probe_battery, BatterySource, UnknownReason,
};
use volttray_protocol::FeatureTable;
use volttray_transport::channel::{HidChannel, IoError};
use volttray_transport::events::{DiagEvent, EventSink};
use volttray_transport::Transport;

struct StubChannel {
    reads: VecDeque<Result<Vec<u8>, IoError>>,
}

impl StubChannel {
    fn scripted(reads: Vec<Result<Vec<u8>, IoError>>) -> Self {
        StubChannel {
            reads: reads.into(),
        }
    }
}

impl HidChannel for StubChannel {
    fn write(&mut self, data: &[u8]) -> Result<usize, IoError> {
        Ok(data.len())
    }
    fn read(&mut self, buf: &mut [u8], _timeout: Duration) -> Result<usize, IoError> {
        match self.reads.pop_front().unwrap_or(Err(IoError::Timeout)) {
            Ok(data) => {
                buf[..data.len()].copy_from_slice(&data);
                Ok(data.len())
            }
            Err(e) => Err(e),
        }
    }
}

#[derive(Default)]
struct VecSink(Vec<DiagEvent>);
impl EventSink for VecSink {
    fn emit(&mut self, event: DiagEvent) {
        self.0.push(event);
    }
}

fn table() -> FeatureTable {
    let mut t = FeatureTable::new();
    t.insert(1, 1); // IFeatureSet
    t.insert(4096, 4); // BATTERY_STATUS
    t.insert(4097, 5); // BATTERY_VOLTAGE
    t.insert(4100, 6); // UNIFIED_BATTERY
    t
}

#[test]
fn mapping_functions() {
    assert_eq!(map_unified_battery(&[0x50, 0x00, 0x01]), Some((80, true)));
    assert_eq!(map_unified_battery(&[0x2D, 0x00, 0x00]), Some((45, false)));
    assert_eq!(map_unified_battery(&[0x65, 0x00, 0x00]), Some((100, false))); // >100 钳制到 100（v0.2.0）
                                                                              // v0.2.0：[percent, nextLevel, state]，state∈{1,2} 为充电中。
    assert_eq!(map_battery_status(&[0x5A, 0x00, 0x01]), Some((90, true)));
    assert_eq!(map_battery_status(&[0x0A, 0x00, 0x00]), Some((10, false)));
    assert_eq!(
        map_battery_voltage(&[0x0F, 0x3C, 0x00], "li-ion"),
        Ok((60, false))
    ); // 3900mV
    assert_eq!(
        map_hidpp1_register_0x0d(&[0x46, 0x00, 0x50]),
        Some((70, true))
    ); // state 0x50 = 充电中
}

#[test]
fn strategy_chain_first_success_wins() {
    let ch = StubChannel::scripted(vec![Ok(vec![0x10, 0x01, 0x06, 0x01, 0x50, 0x00, 0x01])]);
    let mut t = Transport::new(ch, VecSink::default());
    let r = probe_battery(&mut t, "dev1", 0x01, &table(), "li-ion", false).expect("reading");
    assert_eq!(r.percent, 80);
    assert!(r.charging);
    assert_eq!(r.source, BatterySource::UnifiedBattery);
}

#[test]
fn strategy_chain_falls_back_on_unsupported() {
    let ch = StubChannel::scripted(vec![
        // unified → ERR_UNSUPPORTED
        Ok(vec![0x10, 0x01, 0xFF, 0x01, 0x06, 0x09, 0x00]),
        // status → OK 30%
        Ok(vec![0x10, 0x01, 0x04, 0x02, 0x1E, 0x00, 0x00]),
    ]);
    let mut t = Transport::new(ch, VecSink::default());
    let r = probe_battery(&mut t, "dev1", 0x01, &table(), "li-ion", false).expect("reading");
    assert_eq!(r.percent, 30);
    assert_eq!(r.source, BatterySource::BatteryStatus);
}

#[test]
fn voltage_strategy_uses_curve() {
    let ch = StubChannel::scripted(vec![
        Ok(vec![0x10, 0x01, 0xFF, 0x01, 0x06, 0x06, 0x00]), // unified: invalid feature
        Ok(vec![0x10, 0x01, 0xFF, 0x02, 0x04, 0x06, 0x00]), // status: invalid feature
        Ok(vec![0x10, 0x01, 0x05, 0x03, 0x0F, 0x3C, 0x00]), // voltage: 3900mV
    ]);
    let mut t = Transport::new(ch, VecSink::default());
    let r = probe_battery(&mut t, "dev1", 0x01, &table(), "li-ion", false).expect("reading");
    assert_eq!(r.percent, 60);
    assert_eq!(r.source, BatterySource::BatteryVoltage);
}

#[test]
fn device_gone_aborts_chain() {
    let ch = StubChannel::scripted(vec![
        Err(IoError::DeviceGone),
        Ok(vec![0x10, 0x01, 0x04, 0x01, 0x1E, 0, 0]),
    ]);
    let mut t = Transport::new(ch, VecSink::default());
    let r = probe_battery(&mut t, "dev1", 0x01, &table(), "li-ion", false);
    assert_eq!(r, Err(UnknownReason::DeviceGone)); // 不继续尝试后续策略
}

#[test]
fn all_strategies_fail_returns_none() {
    let ch = StubChannel::scripted(vec![
        Ok(vec![0x10, 0x01, 0xFF, 0x01, 0x06, 0x09, 0x00]),
        Ok(vec![0x10, 0x01, 0xFF, 0x02, 0x04, 0x09, 0x00]),
        Ok(vec![0x10, 0x01, 0xFF, 0x03, 0x05, 0x09, 0x00]),
    ]);
    let mut t = Transport::new(ch, VecSink::default());
    assert_eq!(
        probe_battery(&mut t, "dev1", 0x01, &table(), "li-ion", false),
        Err(UnknownReason::ErrorResponse)
    );
}

#[test]
fn hidpp1_device_uses_register() {
    // v0.2.0 请求带 0x81 子地址，响应回显 byte2=0x81、byte3=寄存器号 0x0D（D-17）。
    let ch = StubChannel::scripted(vec![Ok(vec![0x10, 0x03, 0x81, 0x0D, 0x46, 0x00, 0x50])]);
    let mut t = Transport::new(ch, VecSink::default());
    let r =
        probe_battery(&mut t, "dev3", 0x03, &FeatureTable::new(), "li-ion", true).expect("reading");
    assert_eq!(r.percent, 70);
    assert!(r.charging);
    assert_eq!(r.source, BatterySource::Hidpp1Register0x0D);
}
