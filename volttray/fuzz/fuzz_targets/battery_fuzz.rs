#![no_main]
//! E16（报告口径 A-R9）：battery 解析链 fuzz——五个纯映射函数任意
//! 输入不得 panic；返回的 percent 恒 ≤ 100（clamp_pct 契约）。

use libfuzzer_sys::fuzz_target;
use volttray_device::battery;

fuzz_target!(|data: &[u8]| {
    if let Some((pct, _)) = battery::map_unified_battery(data) {
        assert!(pct <= 100, "unified percent 越界：{pct}");
    }
    if let Some((pct, _)) = battery::map_battery_status(data) {
        assert!(pct <= 100, "status percent 越界：{pct}");
    }
    // curve 名从输入派生：已知曲线与 CurveMissing 两路都走到。
    let curve = match data.first().map(|b| b % 3) {
        Some(0) => "li-ion",
        Some(1) => "li-ion-logi",
        _ => "fuzz-unknown-curve",
    };
    if let Ok((pct, _)) = battery::map_battery_voltage(data, curve) {
        assert!(pct <= 100, "voltage percent 越界：{pct}");
    }
    if let Some((pct, _)) = battery::map_hidpp1_register_0x0d(data) {
        assert!(pct <= 100, "0x0d percent 越界：{pct}");
    }
    if let Some((pct, _)) = battery::map_hidpp1_register_0x07(data) {
        assert!(pct <= 100, "0x07 percent 越界：{pct}");
    }
});
