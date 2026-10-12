//! 畸形帧与边界输入的直接单元测试（黄金样例之外的补充）。

use volttray_protocol::error::{DeviceErrorCode, HidppError, MalformedKind};
use volttray_protocol::frame::Frame;

#[test]
fn empty_input() {
    assert_eq!(
        Frame::decode(&[]),
        Err(HidppError::Malformed(MalformedKind::EmptyFrame))
    );
}

#[test]
fn bad_report_ids() {
    for id in [0x00u8, 0x01, 0x0F, 0x12, 0x20, 0xFF] {
        assert_eq!(
            Frame::decode(&[id, 0, 0, 0, 0, 0, 0]),
            Err(HidppError::Malformed(MalformedKind::BadReportId(id)))
        );
    }
}

#[test]
fn length_mismatch_both_directions() {
    assert!(matches!(
        Frame::decode(&[0x10, 0, 0, 0, 0, 0]),
        Err(HidppError::Malformed(MalformedKind::BadLength {
            expected: 7,
            actual: 6,
            ..
        }))
    ));
    assert!(matches!(
        Frame::decode(&[0x10, 0, 0, 0, 0, 0, 0, 0]),
        Err(HidppError::Malformed(MalformedKind::BadLength {
            expected: 7,
            actual: 8,
            ..
        }))
    ));
    assert!(matches!(
        Frame::decode(&[0x11, 0, 0, 0, 0, 0, 0]),
        Err(HidppError::Malformed(MalformedKind::BadLength {
            expected: 20,
            actual: 7,
            ..
        }))
    ));
}

#[test]
fn error_codes_roundtrip() {
    for code in 0x01u8..=0x09 {
        let c = DeviceErrorCode::from_code(code);
        assert_eq!(c.code(), code);
        assert!(!c.name().is_empty());
    }
    assert_eq!(
        DeviceErrorCode::from_code(0x00),
        DeviceErrorCode::UnknownCode(0x00)
    );
    assert_eq!(
        DeviceErrorCode::from_code(0xAB),
        DeviceErrorCode::UnknownCode(0xAB)
    );
}

#[test]
fn long_frame_error_variant() {
    let mut bytes = vec![0x11, 0x09, 0xFF, 0x21, 0x04, 0x08];
    bytes.extend_from_slice(&[0u8; 14]);
    let err = Frame::decode(&bytes).expect_err("device error");
    match err {
        HidppError::Device {
            device_index,
            feature_index,
            code,
            family: _,
        } => {
            assert_eq!(device_index, 0x09);
            assert_eq!(feature_index, 0x04);
            assert_eq!(code, DeviceErrorCode::Busy);
        }
        other => panic!("{other:?}"),
    }
}
