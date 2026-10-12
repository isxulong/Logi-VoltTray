//! proptest 性质测试：decode 对任意输入不 panic；encode→decode 往返一致。

use proptest::prelude::*;
use volttray_protocol::error::HidppError;
use volttray_protocol::frame::{Frame, ERROR_FEATURE_1_0, ERROR_FEATURE_2_0, LONG_LEN, SHORT_LEN};

proptest! {
    /// 任意字节序列 decode 不得 panic（结果可以是任何 Err）。
    #[test]
    fn decode_never_panics(bytes in proptest::collection::vec(any::<u8>(), 0..64)) {
        let _ = Frame::decode(&bytes);
    }

    /// 正常帧（非错误 feature）encode→decode 往返一致。
    #[test]
    fn frame_roundtrip(
        dev in any::<u8>(),
        feat in 0x00u8..=0xFE,
        func in any::<u8>(),
        sw in any::<u8>(),
        long in any::<bool>(),
        params in proptest::collection::vec(any::<u8>(), 16..=16),
    ) {
        prop_assume!(feat != ERROR_FEATURE_2_0 && feat != ERROR_FEATURE_1_0);
        let frame = if long {
            let mut p = [0u8; 16];
            p.copy_from_slice(&params);
            Frame::new_long(dev, feat, func, sw, p)
        } else {
            Frame::new_short(dev, feat, func, sw, [params[0], params[1], params[2]])
        };
        let encoded = frame.encode();
        prop_assert_eq!(encoded.len(), if long { LONG_LEN } else { SHORT_LEN });
        let decoded = Frame::decode(&encoded).map_err(|e| TestCaseError::fail(format!("decode: {e}")))?;
        prop_assert_eq!(decoded.device_index, dev);
        prop_assert_eq!(decoded.feature_index, feat);
        prop_assert_eq!(decoded.function, func & 0x0F);
        prop_assert_eq!(decoded.sw_id, sw & 0x0F);
        prop_assert_eq!(decoded.params(), frame.params());
    }

    /// function / sw_id 超 4 bit 时被截断（不溢出到对方字段）。
    #[test]
    fn nibble_masking(func in 0x10u8..=0xFF, sw in 0x10u8..=0xFF) {
        let f = Frame::new_short(1, 2, func, sw, [0; 3]);
        prop_assert_eq!(f.function, func & 0x0F);
        prop_assert_eq!(f.sw_id, sw & 0x0F);
        let bytes = f.encode();
        prop_assert_eq!(bytes[3], ((func & 0x0F) << 4) | (sw & 0x0F));
    }

    /// 错误帧的 9 个规范码往返。
    #[test]
    fn error_frame_roundtrip(code in 0x01u8..=0x09, dev in any::<u8>(), feat in any::<u8>()) {
        prop_assume!(feat != ERROR_FEATURE_2_0 && feat != ERROR_FEATURE_1_0);
        // v0.2.0 错误帧布局：byte3=feat 回显，byte4=fn|sw，byte5=code。
        let bytes = [0x10, dev, ERROR_FEATURE_2_0, feat, 0, code, 0];
        match Frame::decode(&bytes) {
            Err(HidppError::Device { code: got, feature_index, .. }) => {
                prop_assert_eq!(got.code(), code);
                prop_assert_eq!(feature_index, feat);
            }
            other => return Err(TestCaseError::fail(format!("{other:?}"))),
        }
    }
}
