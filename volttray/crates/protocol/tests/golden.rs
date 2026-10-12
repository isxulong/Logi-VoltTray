//! 协议黄金样例测试：fixtures/golden/*.json 全量回放。
//! 样例 schema 见 fixtures/gen_golden.py 头部注释。

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::PathBuf;
use volttray_protocol::commands::QueryCommand;
use volttray_protocol::error::{DeviceErrorCode, ErrorFamily, HidppError, MalformedKind};
use volttray_protocol::frame::Frame;
use volttray_protocol::FeatureTable;

#[derive(Deserialize)]
struct Sample {
    name: String,
    kind: String,
    #[serde(default)]
    hex: String,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    device_index: Option<u8>,
    #[serde(default)]
    sw_id: Option<u8>,
    #[serde(default)]
    features: BTreeMap<String, u8>,
    expect: serde_json::Value,
}

fn parse_hex(s: &str) -> Vec<u8> {
    s.split_whitespace()
        .map(|t| u8::from_str_radix(t, 16).unwrap_or_else(|_| panic!("bad hex byte {t:?}")))
        .collect()
}

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/golden")
}

#[test]
fn golden_samples_all_pass() {
    let dir = golden_dir();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("golden dir")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    assert!(
        files.len() >= 40,
        "golden sample count too low: {}",
        files.len()
    );

    for path in &files {
        let text = std::fs::read_to_string(path).expect("read sample");
        let sample: Sample =
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let fname = path.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(
            sample.name,
            fname.trim_end_matches(".json"),
            "{fname}: name mismatch"
        );
        match sample.kind.as_str() {
            "response" => check_response(&fname, &sample),
            "malformed" => check_malformed(&fname, &sample),
            "request" => check_request(&fname, &sample),
            other => panic!("{fname}: unknown kind {other}"),
        }
    }
    println!("golden samples passed: {}", files.len());
}

fn check_response(fname: &str, sample: &Sample) {
    let bytes = parse_hex(&sample.hex);
    let expect = &sample.expect;
    match expect["type"].as_str().expect("type") {
        "frame" => {
            let frame =
                Frame::decode(&bytes).unwrap_or_else(|e| panic!("{fname}: decode failed: {e}"));
            assert_eq!(
                frame.report,
                expect["report"].as_u64().unwrap() as u8,
                "{fname} report"
            );
            assert_eq!(
                frame.device_index,
                expect["device_index"].as_u64().unwrap() as u8,
                "{fname} dev"
            );
            assert_eq!(
                frame.feature_index,
                expect["feature_index"].as_u64().unwrap() as u8,
                "{fname} feat"
            );
            assert_eq!(
                frame.function,
                expect["function"].as_u64().unwrap() as u8,
                "{fname} func"
            );
            assert_eq!(
                frame.sw_id,
                expect["sw_id"].as_u64().unwrap() as u8,
                "{fname} sw"
            );
            assert_eq!(
                frame.params(),
                parse_hex(expect["params_hex"].as_str().unwrap()).as_slice(),
                "{fname} params"
            );
        }
        "error" => {
            let err = Frame::decode(&bytes).expect_err(&format!("{fname}: must be device error"));
            match err {
                HidppError::Device {
                    device_index,
                    family,
                    feature_index,
                    code,
                } => {
                    assert_eq!(
                        device_index,
                        expect["device_index"].as_u64().unwrap() as u8,
                        "{fname} dev"
                    );
                    // v0.2.0：错误帧家族是红队差异 F5 的核心——同码不同义。
                    if let Some(want) = expect["family"].as_str() {
                        assert_eq!(family.name(), want, "{fname} family");
                    }
                    assert_eq!(
                        feature_index,
                        expect["feature_index"].as_u64().unwrap() as u8,
                        "{fname} feat"
                    );
                    let want = expect["error_code"].as_u64().unwrap() as u8;
                    assert_eq!(code.code(), want, "{fname} code");
                    if (0x01..=0x09).contains(&want) {
                        assert!(
                            !matches!(code, DeviceErrorCode::UnknownCode(_)),
                            "{fname}: known code misclassified"
                        );
                    } else {
                        assert!(
                            matches!(code, DeviceErrorCode::UnknownCode(_)),
                            "{fname}: unknown code must keep raw"
                        );
                    }
                }
                other => panic!("{fname}: unexpected error {other:?}"),
            }
        }
        other => panic!("{fname}: unknown expect type {other}"),
    }
}

fn check_malformed(fname: &str, sample: &Sample) {
    let bytes = parse_hex(&sample.hex);
    let err = Frame::decode(&bytes).expect_err(&format!("{fname}: must be malformed"));
    let HidppError::Malformed(kind) = err else {
        panic!("{fname}: expected Malformed, got {err:?}");
    };
    let expect = &sample.expect;
    match (expect["kind"].as_str().unwrap(), kind) {
        ("EmptyFrame", MalformedKind::EmptyFrame) => {}
        ("BadReportId", MalformedKind::BadReportId(id)) => {
            assert_eq!(id, expect["report"].as_u64().unwrap() as u8, "{fname}");
        }
        (
            "BadLength",
            MalformedKind::BadLength {
                report,
                expected,
                actual,
            },
        ) => {
            assert_eq!(report, expect["report"].as_u64().unwrap() as u8, "{fname}");
            assert_eq!(
                expected,
                expect["expected"].as_u64().unwrap() as usize,
                "{fname}"
            );
            assert_eq!(
                actual,
                expect["actual"].as_u64().unwrap() as usize,
                "{fname}"
            );
        }
        (want, got) => panic!("{fname}: want {want}, got {got:?}"),
    }
}

fn check_request(fname: &str, sample: &Sample) {
    let command = match sample.command.as_deref().expect("command") {
        "GetProtocolVersion" => QueryCommand::GetProtocolVersion,
        "GetFeatureIndex" => QueryCommand::GetFeatureIndex { feature_id: 0x1004 },
        "GetBatteryUnified" => QueryCommand::GetBatteryUnified,
        "GetBatteryStatus" => QueryCommand::GetBatteryStatus,
        "GetBatteryVoltage" => QueryCommand::GetBatteryVoltage,
        "ReadHidpp1Register0x0D" => QueryCommand::ReadHidpp1Register0x0D,
        "ReadHidpp1Register0x07" => QueryCommand::ReadHidpp1Register0x07,
        other => panic!("{fname}: unknown command {other}"),
    };
    let mut table = FeatureTable::new();
    for (k, v) in &sample.features {
        table.insert(k.parse::<u16>().expect("feature id"), *v);
    }
    let bytes = command
        .encode_for(
            sample.device_index.expect("dev"),
            sample.sw_id.expect("sw"),
            &table,
        )
        .unwrap_or_else(|e| panic!("{fname}: encode failed: {e}"));
    assert_eq!(
        bytes,
        parse_hex(sample.expect["hex"].as_str().unwrap()),
        "{fname}: encode bytes"
    );
}
