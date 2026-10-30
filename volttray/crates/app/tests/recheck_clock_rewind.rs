//! 质检 F1 对账回归：回拨尾段 / 对抗样本两类自建固件，Rust 导出三件套
//! 与 tools/recheck.py 逐值复算对账（AC-B8 口径，匿名化开/关两态）。
//!
//! python3 不可用时跳过（打印提示），可用时必须 exit=0。

use std::path::{Path, PathBuf};
use std::process::Command;

use volttray_app::attribution::{
    anonymize_map, build_report_data, build_segments, event_keys, render_csv, render_html,
    render_json_events,
};
use volttray_app::stats::load_events_from;

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root")
}

fn tmpdir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "volttray-recheck-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 单个固件 × 匿名化两态对账：Rust 导出 → recheck.py 复算，exit 必须为 0。
fn run_recheck(fixture: &Path, anon: bool) -> Result<(), String> {
    let events = load_events_from(Path::new("/dev/null"), fixture);
    assert!(!events.is_empty(), "fixture {:?} 无事件", fixture);
    let segments = build_segments(&events);
    let anon_map = if anon {
        Some(anonymize_map(&event_keys(&events)))
    } else {
        None
    };
    let tz = 480;
    let generated = 1_760_745_600i64; // 固定生成时刻（对账确定性）
    let dir = tmpdir(&format!(
        "{}-anon{}",
        fixture.file_stem().unwrap().to_string_lossy(),
        anon as u8
    ));
    let csv_path = dir.join("out.csv");
    std::fs::write(&csv_path, render_csv(&segments, anon_map.as_ref(), tz, generated)).unwrap();
    let report = build_report_data(&events, &segments, anon_map.as_ref(), tz, generated);
    std::fs::write(dir.join("out.html"), render_html(&report)).unwrap();
    std::fs::write(
        dir.join("out.json"),
        render_json_events(&events, anon_map.as_ref(), generated),
    )
    .unwrap();

    let script = repo_root().join("tools/recheck.py");
    let out = Command::new("python3")
        .arg(&script)
        .arg("--stats")
        .arg(fixture)
        .arg("--csv")
        .arg(&csv_path)
        .arg("--html")
        .arg(dir.join("out.html"))
        .arg("--json")
        .arg(dir.join("out.json"))
        .arg("--tz")
        .arg(tz.to_string())
        .output()
        .map_err(|e| format!("spawn python3 failed: {e}"))?;
    let _ = std::fs::remove_dir_all(&dir);
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "recheck.py exit={:?}\nstderr: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        ))
    }
}

fn python3_available() -> bool {
    Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn clock_rewind_tail_fixture_matches_recheck_py() {
    if !python3_available() {
        eprintln!("python3 不可用，跳过 recheck 对账（时钟回拨尾段固件）");
        return;
    }
    let fixture = repo_root().join("fixtures/stats_clock_rewind_tail.jsonl");
    for anon in [false, true] {
        run_recheck(&fixture, anon)
            .unwrap_or_else(|e| panic!("回拨尾段固件对账失败（anon={anon}）：{e}"));
    }
}

#[test]
fn adversarial_fixture_matches_recheck_py() {
    if !python3_available() {
        eprintln!("python3 不可用，跳过 recheck 对账（对抗样本固件）");
        return;
    }
    let fixture = repo_root().join("fixtures/stats_adversarial.jsonl");
    for anon in [false, true] {
        run_recheck(&fixture, anon)
            .unwrap_or_else(|e| panic!("对抗样本固件对账失败（anon={anon}）：{e}"));
    }
}
