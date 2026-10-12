//! app 层集成测试：配置热重载（mtime 轮询）与观测流联动。

use std::io::Write;
use volttray_app::config::{Config, ConfigWatcher};
use volttray_app::observer::{Observer, SharedObserver};
use volttray_transport::events::{DiagEvent, DiagKind, EventSink, Layer};

fn temp_config(name: &str, content: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!(
        "volttray_test_{}_{}.toml",
        name,
        std::process::id()
    ));
    let mut f = std::fs::File::create(&path).expect("create temp config");
    f.write_all(content.as_bytes()).expect("write temp config");
    path
}

#[test]
fn watcher_detects_mtime_change() {
    let path = temp_config("watch", "low_threshold = 30\n");
    let mut watcher = ConfigWatcher::new(path.clone());
    assert!(watcher.poll().is_none(), "no change yet");

    // 确保 mtime 前进（部分文件系统粒度为秒级）。
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&path, "low_threshold = 33\n").expect("rewrite");

    let loaded = watcher.poll().expect("change detected").expect("load ok");
    assert_eq!(loaded.0.low_threshold, 33);
    assert!(loaded.1.is_empty());
    assert!(watcher.poll().is_none(), "no further change");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn watcher_survives_invalid_rewrite() {
    let path = temp_config("watch_bad", "low_threshold = 30\n");
    let mut watcher = ConfigWatcher::new(path.clone());
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(&path, "low_threshold = \"abc\"\nunknown_key = 1\n").expect("rewrite");
    let (cfg, warnings) = watcher.poll().expect("change").expect("load ok");
    assert_eq!(
        cfg,
        Config::default(),
        "invalid values fall back to defaults"
    );
    assert_eq!(warnings.len(), 2);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn shared_observer_receives_events_from_sink() {
    let shared = SharedObserver::new();
    let mut sink = shared.clone();
    sink.emit(DiagEvent::new(
        Layer::Tray,
        DiagKind::Info {
            message: "hello".into(),
        },
    ));
    let obs = shared.0.lock().expect("lock");
    assert_eq!(obs.len(), 1);
    let e = obs.events().next().expect("event");
    assert_eq!(e.seq, 1);
    assert!(e.ts_ms > 0, "timestamp stamped by observer");
}

#[test]
fn config_reloaded_event_carries_warnings() {
    let mut obs = Observer::new();
    let warnings = vec!["config key \"x\": unknown key; ignored".to_string()];
    obs.push(DiagEvent::new(
        Layer::App,
        DiagKind::ConfigReloaded { warnings },
    ));
    let json = obs.export_json(true);
    assert!(json.contains("config_reloaded"));
    assert!(json.contains("unknown key"));
}
