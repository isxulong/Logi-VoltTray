//! 帧解析 fuzz target（评审 P0-2 骨架）：decode 永不 panic、错误分类稳定。
#![no_main]

use libfuzzer_sys::fuzz_target;
use volttray_protocol::Frame;

fuzz_target!(|data: &[u8]| {
    // 不变量：任意输入不得 panic；能 decode 的必须能再 encode。
    if let Ok(frame) = Frame::decode(data) {
        let bytes = frame.encode();
        let _ = Frame::decode(&bytes);
    }
});
