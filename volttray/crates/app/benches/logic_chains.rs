//! E20（红蓝对抗终审 P2b 批，2026-10-24）：纯逻辑链微基准②③——
//! redact_text 结构化脱敏 / StatsEvent to_json_line→parse_line roundtrip。
//! 只测纯计算路径，零 I/O；首版只跑数不设阈值门禁（见 ci.yml
//! microbench job 注释）。

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use volttray_app::redact::redact_text;
use volttray_app::stats::{ReadEvent, StatsEvent, SOURCE_POLL};

/// 拼一份近似诊断导出规模的输入：含用户路径与多台设备名，
/// 覆盖规则一（用户路径）与规则二（设备名整值替换优先）。
fn diag_like_input() -> (String, Vec<String>) {
    let names = vec![
        "MX Keys".to_string(),
        "MX Keys Mini".to_string(),
        "G502 LIGHTSPEED".to_string(),
    ];
    let mut s = String::with_capacity(8 * 1024);
    for i in 0..40 {
        s.push_str(&format!(
            "line {i}: C:\\Users\\zhangsan\\AppData\\volttray\\log 设备 MX Keys Mini 读数 87%（G502 LIGHTSPEED 充电中）；path=C:\\Users\\zhangsan\\x\\{i}.log\n"
        ));
    }
    (s, names)
}

fn read_event() -> StatsEvent {
    StatsEvent::Read(ReadEvent {
        ts: 1_761_200_000,
        physical_key: "usb-receiver:1".to_string(),
        display_name: "MX Keys".to_string(),
        percent: 87,
        charging: false,
        source: SOURCE_POLL.to_string(),
    })
}

fn bench_redact(c: &mut Criterion) {
    let (input, names) = diag_like_input();
    c.bench_function("redact_text_diag_like", |b| {
        b.iter(|| redact_text(black_box(&input), black_box(&names)))
    });
}

fn bench_stats_roundtrip(c: &mut Criterion) {
    let mut g = c.benchmark_group("stats_jsonl");
    g.bench_function("read_event_json_roundtrip", |b| {
        b.iter(|| {
            let line = read_event().to_json_line();
            StatsEvent::parse_line(black_box(&line)).expect("自产行必须可解析")
        })
    });
    g.bench_function("read_event_parse_only", |b| {
        let line = read_event().to_json_line();
        b.iter(|| StatsEvent::parse_line(black_box(&line)).expect("固定合法行必须可解析"))
    });
    g.finish();
}

criterion_group!(benches, bench_redact, bench_stats_roundtrip);
criterion_main!(benches);
