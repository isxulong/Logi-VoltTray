#![no_main]
//! E16（报告口径 A-R9）：stats 解析链 fuzz——任意字节行不得 panic；
//! 解析成功的事件 roundtrip（to_json_line → parse_line）必须仍
//! 解析成功（skip-bad-lines 口径下，自家产出必须是良行）。

use libfuzzer_sys::fuzz_target;
use volttray_app::stats::StatsEvent;

fuzz_target!(|data: &[u8]| {
    let line = String::from_utf8_lossy(data);
    if let Some(ev) = StatsEvent::parse_line(&line) {
        let out = ev.to_json_line();
        assert!(
            StatsEvent::parse_line(&out).is_some(),
            "roundtrip 后必须仍可解析：{out}"
        );
    }
});
