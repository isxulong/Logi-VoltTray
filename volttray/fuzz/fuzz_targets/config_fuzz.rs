#![no_main]
//! E16（报告口径 A-R9）：config 解析链 fuzz——任意文本不得 panic；
//! 解析后不变量（normalize 兜底契约，与 Config::parse 尾部规范化
//! 逐条对应）：档数在 MIN..=MAX、阈值严格递增且全在 1..=99、
//! 轮询间隔吸附到 POLL_TIERS_SECS、低电阈值钳在 5..=50。

use libfuzzer_sys::fuzz_target;
use volttray_app::config::{
    Config, FONT_TIER_MAX_TIERS, FONT_TIER_MIN_TIERS, FONT_TIER_THRESHOLD_MAX,
    FONT_TIER_THRESHOLD_MIN, LOW_THRESHOLD_MAX, LOW_THRESHOLD_MIN, POLL_TIERS_SECS,
};

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let (cfg, _warnings) = Config::parse(&text);
    let ths = &cfg.icon_font_tier_thresholds;
    assert!(
        (FONT_TIER_MIN_TIERS..=FONT_TIER_MAX_TIERS).contains(&(ths.len() + 1)),
        "档数越界：{}",
        ths.len() + 1
    );
    assert!(
        ths.iter()
            .all(|t| (FONT_TIER_THRESHOLD_MIN..=FONT_TIER_THRESHOLD_MAX).contains(t)),
        "阈值越界：{ths:?}"
    );
    assert!(
        ths.windows(2).all(|w| w[0] < w[1]),
        "阈值非严格递增：{ths:?}"
    );
    assert!(
        POLL_TIERS_SECS.contains(&cfg.poll_interval_secs),
        "轮询间隔未吸附档位：{}",
        cfg.poll_interval_secs
    );
    assert!(
        (LOW_THRESHOLD_MIN..=LOW_THRESHOLD_MAX).contains(&cfg.low_threshold),
        "低电阈值越界：{}",
        cfg.low_threshold
    );
});
