//! v0.2.0 电量映射黄金用例：消费 `fixtures/battery_map_cases.json`。
//!
//! 数据来源：红蓝对抗产出的协议实证清单 + 参考实现真机读数
//! （仅采纳协议与测试实证，不采纳其架构）。覆盖四组：
//! - `voltage_curve_liion_logi`：真机 13 锚点曲线锚值 / 用例 / 单调扫掠；
//! - `clamp_pct`：>100 钳制；
//! - `battery_paths`：五条电量路径的参数 → (百分比, 充电中)；
//! - `clean_name`：设备名原始字节 → 消毒输出。
//!
//! `transport_matching` 4 条不在此消费：`response_matches` 与握手分类是
//! transport / device::handshake 的私有实现细节，已由各自 crate 内单元测试
//! 逐条覆盖（transport/src/lib.rs tests、device/src/handshake.rs tests）。
//!
//! 与蓝队样本的两处有意偏差（既有安全裁决，不因样本更改）：
//! 1. `clean_name` 空串：样本期望 `""`；本实现按评审 P1-5 消毒裁决回退
//!    「未知设备」——UI 层不允许出现空设备名；
//! 2. `clean_name` 样本中 `0x00` 之后的字节：协议层读名按名称长度截断
//!    （0x0005 fn0 返回长度），填充字节不进入消毒层——本测试先按首个
//!    `0x00` 截断再消毒，与真机读数路径一致。

use serde_json::Value;

use volttray_device::battery::{
    clamp_pct, map_battery_status, map_battery_voltage, map_hidpp1_register_0x0d,
    map_hidpp1_register_0x07, map_unified_battery,
};
use volttray_device::sanitize;
use volttray_protocol::voltage;

const CASES: &str = include_str!("../../../fixtures/battery_map_cases.json");

fn root() -> Value {
    serde_json::from_str(CASES).expect("battery_map_cases.json must be valid JSON")
}

/// 五条电量路径统一分派（battery_voltage 固定用真机曲线 li-ion-logi）。
fn apply_path(source: &str, params: &[u8]) -> Option<(u8, bool)> {
    match source {
        "unified_battery" => map_unified_battery(params),
        "battery_status" => map_battery_status(params),
        "battery_voltage" => map_battery_voltage(params, "li-ion-logi").ok(),
        "hidpp1_reg_0d" => map_hidpp1_register_0x0d(params),
        "hidpp1_reg_07" => map_hidpp1_register_0x07(params),
        other => panic!("unknown battery path source: {other}"),
    }
}

#[test]
fn liion_logi_voltage_curve_cases() {
    let root = root();
    let sec = &root["voltage_curve_liion_logi"];
    let points: Vec<(u16, u8)> = sec["points"]
        .as_array()
        .expect("points array")
        .iter()
        .map(|p| {
            (
                p[0].as_u64().expect("mv") as u16,
                p[1].as_u64().expect("pct") as u8,
            )
        })
        .collect();
    // 真机 13 锚点曲线（参考实现实证），少于 13 条视为数据缺失。
    assert!(points.len() >= 13, "li-ion-logi anchors: {}", points.len());
    // 锚点必须与 fixtures/voltage_curves.json 里的 li-ion-logi 完全一致。
    for (mv, pct) in &points {
        assert_eq!(voltage::percent_from_mv("li-ion-logi", *mv), Some(*pct), "{mv}mV");
    }
    // 真机读数用例。
    for case in sec["cases"].as_array().expect("cases array") {
        let mv = case["mv"].as_u64().expect("mv") as u16;
        let pct = case["pct"].as_u64().expect("pct") as u8;
        assert_eq!(
            voltage::percent_from_mv("li-ion-logi", mv),
            Some(pct),
            "{mv}mV"
        );
    }
    // 单调扫掠：毫伏升 → 百分比不降（红队：曲线必须单调）。
    let sweep = sec["monotonic_sweep_mv"].as_array().expect("sweep array");
    let (lo, hi, step) = (
        sweep[0].as_u64().expect("lo") as u16,
        sweep[1].as_u64().expect("hi") as u16,
        sweep[2].as_u64().expect("step"),
    );
    let mut last = 0u8;
    for mv in (lo..=hi).step_by(step.max(1) as usize) {
        let p = voltage::percent_from_mv("li-ion-logi", mv).expect("curve");
        assert!(p >= last, "not monotonic at {mv}mV: {p} < {last}");
        last = p;
    }
}

#[test]
fn clamp_pct_cases() {
    let root = root();
    let cases = root["clamp_pct"].as_array().expect("clamp_pct array");
    assert!(cases.len() >= 5, "clamp cases: {}", cases.len());
    for case in cases {
        let input = case["in"].as_u64().expect("in") as u8;
        let out = case["out"].as_u64().expect("out") as u8;
        assert_eq!(clamp_pct(input), out, "clamp_pct({input})");
    }
}

#[test]
fn battery_path_cases() {
    let root = root();
    let cases = root["battery_paths"].as_array().expect("battery_paths array");
    assert!(cases.len() >= 15, "battery path cases: {}", cases.len());
    for case in cases {
        let source = case["source"].as_str().expect("source");
        let params: Vec<u8> = case["params"]
            .as_array()
            .expect("params array")
            .iter()
            .map(|v| v.as_u64().expect("param byte") as u8)
            .collect();
        let got = apply_path(source, &params);
        // percent == null 表示映射必须失败（缺参数）。
        let want = match case["percent"].as_u64() {
            Some(pct) => {
                let charging = case["charging"].as_bool().expect("charging");
                Some((pct as u8, charging))
            }
            None => None,
        };
        assert_eq!(got, want, "source={source} params={params:02x?}");
        // 电压路径顺带核验毫伏换算。
        if source == "battery_voltage" {
            if let Some(mv) = case["mv"].as_u64() {
                let computed = u16::from_be_bytes([params[0], params[1]]) as u64;
                assert_eq!(computed, mv, "{source} mv");
            }
        }
    }
}

#[test]
fn clean_name_cases() {
    let root = root();
    let cases = root["clean_name"].as_array().expect("clean_name array");
    assert!(cases.len() >= 5, "clean_name cases: {}", cases.len());
    for case in cases {
        let raw: Vec<u8> = case["in"]
            .as_str()
            .expect("hex string")
            .split_whitespace()
            .map(|t| u8::from_str_radix(t, 16).expect("hex byte"))
            .collect();
        // 偏差 2：0x00 后为协议填充字节，按读名长度截断。
        let cut = raw.split(|&b| b == 0).next().unwrap_or(&[]);
        let s = String::from_utf8_lossy(cut);
        let out = sanitize::device_name(&s);
        if s.is_empty() {
            // 偏差 1：P1-5 裁决——空名回退「未知设备」，非空串。
            assert_eq!(out, sanitize::FALLBACK_DEVICE_NAME, "empty name");
        } else {
            assert_eq!(out, case["out"].as_str().expect("out"), "raw={s:?}");
        }
    }
}
