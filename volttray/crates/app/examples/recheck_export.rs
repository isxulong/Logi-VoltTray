//! WP-B（I-08）配套：从 jsonl 直接生成 CSV / HTML / JSON 导出（与托盘
//! 菜单导出同一代码路径），供 tools/recheck.py 复算对账（AC-B2/B8）。
//!
//! 用法：
//!   recheck_export <stats.jsonl> <archive.jsonl|-> <out.csv> <tz_minutes> <anon 0|1>
//! 产物：<out.csv>、同主干 <out.html>、<out.json>（三件套齐出）。

use std::path::Path;

use volttray_app::attribution::{
    anonymize_map, build_report_data, build_segments, event_keys, render_csv, render_html,
    render_json_events,
};
use volttray_app::stats::load_events_from;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 6 {
        eprintln!("usage: recheck_export <stats.jsonl> <archive|-> <out.csv> <tz_min> <anon 0|1>");
        std::process::exit(2);
    }
    let archive = if args[2] == "-" {
        "/dev/null"
    } else {
        &args[2]
    };
    let events = load_events_from(Path::new(archive), Path::new(&args[1]));
    let segments = build_segments(&events);
    let anon = if args[5] == "1" {
        Some(anonymize_map(&event_keys(&events)))
    } else {
        None
    };
    let tz: i32 = args[4].parse().expect("tz minutes");
    let csv = render_csv(&segments, anon.as_ref(), tz);
    std::fs::write(&args[3], &csv).expect("write csv");
    let stem = Path::new(&args[3]).with_extension("");
    let generated = 1_760_745_600i64; // 固定生成时刻（对账确定性）
    let report = build_report_data(&events, &segments, anon.as_ref(), tz, generated);
    std::fs::write(stem.with_extension("html"), render_html(&report)).expect("write html");
    let json = render_json_events(&events, anon.as_ref(), generated);
    std::fs::write(stem.with_extension("json"), json).expect("write json");
    println!(
        "events={} segments={} -> {}(.csv/.html/.json)",
        events.len(),
        segments.len(),
        args[3]
    );
}
