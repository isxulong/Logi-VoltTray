//! E8（红蓝对抗终审）：自 main.rs 拆出的导出叶子模块。
//!
//! 仅结构搬动（模块归属、可见性与遥测读取穿线调整），函数体逐字节不变
//! ——行为基线以合成 HID 序列 + 抽取函数逐字节比对验证。包含：os error
//! 可读化、诊断导出落盘（目录自建 + Save-As 对话框 + 不做自动清理）、
//! 耗电统计三件套导出（CSV/JSON/HTML，tmp→rename 原子写）。帧级 / 唤醒
//! 遥测计数经 Telemetry（由 run 持有）读取，取代原进程级静态量。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::Controls::Dialogs::{
    CommDlgExtendedError, GetSaveFileNameW, OFN_NOCHANGEDIR, OFN_OVERWRITEPROMPT,
    OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONINFORMATION, MB_OK};

use volttray_app::diag_export::{
    diag_export_filename_local, read_log_tail, DiagExport, EnumSnapshot, LocalTimeParts,
    LOG_TAIL_MAX_LINES,
};
use volttray_app::Observer;
use volttray_transport::events::DiagKind;

use crate::bootlog;
use crate::persist::write_tmp_rename;
use crate::{wall_secs, wide, Telemetry};

/// WP-B 统计导出（IO 线程后台执行，v3 P2-③）：导出前先强制 flush
///（缓冲事件不落空，v3「导出前先 flush」）→ 读双文件（current +
/// archive）→ 分段归因 →（可选）FNV-1a 哈希序匿名化 → 渲染。
/// CSV 主产物写用户在 Save-As 确认的路径（E5 起同样走 tmp→rename
/// 原子写，写中途失败不留半截 CSV）；JSON / HTML 主干跟随 CSV 最终
/// 确认文件名（v3 L111）；三件套各自 tmp→rename 落盘、失败清 tmp、
/// 互不影响。只读本机文件 + 渲染写盘，零设备查询（红线）。
/// 返回给用户的结果文案（成功列文件、失败给原因、部分成功如实分列）。
pub(crate) fn run_stats_export(
    store: &mut volttray_app::stats::StatsStore,
    req: &crate::statsexport::StatsExportRequest,
) -> String {
    use volttray_app::attribution as attr;
    if let Err(e) = store.shutdown_flush(Instant::now(), wall_secs() as i64) {
        bootlog::note(&format!("stats export: pre-export flush failed: {e}"));
    }
    let mut events = volttray_app::stats::load_events_from(store.archive_path(), store.path());
    // 事件流闸门单点谓词（attribution::export_event_allowed，与本
    // crate golden 测试同口径）：activity_buckets 逐分钟位图须
    // 融合稿 Phase 1「含活动统计」（include_activity，默认开）与
    // 组4 N-08② 位图独立开关（export_activity_bitmap，默认关，
    // 与 anonymize 解耦——作息泄露与是否匿名无关）同时开才放行；
    // 其余事件恒放行。编排（flush→load→分段→渲染）不变，仅此一处
    // 数据选择。
    events.retain(|e| {
        attr::export_event_allowed(e, req.include_activity, req.export_activity_bitmap)
    });
    let segments = attr::build_segments(&events);
    let anon = if req.anonymize {
        Some(attr::anonymize_map(&attr::event_keys(&events)))
    } else {
        None
    };
    // 组5 5b（C-3）：anonymize=on 且脱敏闸门开 → display_name 会话
    // 内映射「设备N」（与诊断面 redact::device_session_map 同函数同
    // 序号）；闸门回退或 anonymize=off → None（保留真实型号，U-2C）。
    let name_map = if req.anonymize && req.redact_display_name {
        Some(volttray_app::redact::device_session_map(
            &attr::display_name_session_order(&events),
        ))
    } else {
        None
    };
    // 组5 5b（U-2 闸门，BvA-5 默认关）：tz_utc=true → 时间戳按 UTC
    //（+00:00）渲染；false → 本地时区（现状，翻转挂 U-2 裁决）。
    let offset = if req.tz_utc {
        0
    } else {
        local_offset_minutes()
    };
    let mut done: Vec<String> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    // E5（P1a）：CSV 主产物同样走 tmp→rename 原子写。
    match write_tmp_rename(
        &req.csv_path,
        &attr::render_csv(
            &segments,
            anon.as_ref(),
            name_map.as_ref(),
            offset,
            wall_secs() as i64,
        ),
    ) {
        Ok(()) => done.push(req.csv_path.display().to_string()),
        Err(e) => failed.push(format!("{}: {e}", req.csv_path.display())),
    }
    let stem = req.csv_path.with_extension("");
    if req.json {
        let p = stem.with_extension("json");
        let content = attr::render_json_events(
            &events,
            anon.as_ref(),
            name_map.as_ref(),
            req.export_activity_bitmap,
            wall_secs() as i64,
        );
        match write_tmp_rename(&p, &content) {
            Ok(()) => done.push(p.display().to_string()),
            Err(e) => failed.push(format!("{}: {e}", p.display())),
        }
    }
    if req.html {
        let p = stem.with_extension("html");
        let data = attr::build_report_data(
            &events,
            &segments,
            anon.as_ref(),
            name_map.as_ref(),
            offset,
            wall_secs() as i64,
        );
        let content = attr::render_html(&data);
        match write_tmp_rename(&p, &content) {
            Ok(()) => done.push(p.display().to_string()),
            Err(e) => failed.push(format!("{}: {e}", p.display())),
        }
    }
    if failed.is_empty() {
        format!("导出完成：\n{}", done.join("\n"))
    } else if done.is_empty() {
        format!("导出失败：\n{}", failed.join("\n"))
    } else {
        format!(
            "部分完成：\n已导出：{}\n失败：{}",
            done.join("；"),
            failed.join("；")
        )
    }
}

/// WP-B：本地时区偏移（分钟；UTC+8 = 480）——CSV/HTML 时间戳渲染用。
/// GetLocalTime vs GetSystemTime 的分钟差（系统已把夏令时折算进本地
/// 时间）；纯算术实现（days-from-civil，与 app 层 civil_from_days 互
/// 逆），不引入额外 Win32 依赖。SYSTEMTIME 两 API 均无失败路径。
pub(crate) fn local_offset_minutes() -> i32 {
    fn st_minutes(st: &windows::Win32::Foundation::SYSTEMTIME) -> i64 {
        let y = st.wYear as i64 - if st.wMonth <= 2 { 1 } else { 0 };
        let era = if y >= 0 { y } else { y - 399 } / 400;
        let yoe = y - era * 400;
        let mp = (st.wMonth as i64 + 9) % 12;
        let doy = (153 * mp + 2) / 5 + st.wDay as i64 - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146097 + doe - 719468;
        days * 1440 + st.wHour as i64 * 60 + st.wMinute as i64
    }
    // SAFETY(E9): GetLocalTime / GetSystemTime 均无参、返回 POD SYSTEMTIME 值，无指针契约。
    let (local, utc) = unsafe {
        (
            GetLocalTime(),
            windows::Win32::System::SystemInformation::GetSystemTime(),
        )
    };
    (st_minutes(&local) - st_minutes(&utc)) as i32
}

/// v0.3.3 #2：把 os error 翻译成可读错误（保留原始错误供诊断）。
pub(crate) fn friendly_io_error(e: &std::io::Error) -> String {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::NotFound => "系统找不到指定的路径（目录不存在）".to_string(),
        ErrorKind::PermissionDenied => "没有写入权限（请检查目录权限）".to_string(),
        _ => e.to_string(),
    }
}

/// 诊断导出落盘（v0.3.3 #2，可测试纯函数）：
/// 导出前 `create_dir_all` 确保目录存在（干净机器无历史目录也能导出），
/// 写入失败返回可读错误（含原始 os error）而非裸错误码。
pub(crate) fn write_diag_export(dir: &Path, filename: &str, json: &str) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| {
        format!(
            "无法创建导出目录 {}：{}；原始错误：{e}",
            dir.display(),
            friendly_io_error(&e)
        )
    })?;
    let path = dir.join(filename);
    std::fs::write(&path, json).map_err(|e| {
        format!(
            "无法写入 {}：{}；原始错误：{e}",
            path.display(),
            friendly_io_error(&e)
        )
    })?;
    Ok(path)
}

/// v0.3.15：「另存为」对话框选导出路径（GetSaveFileNameW）。
fn pick_save_path(default_name: &str) -> Option<PathBuf> {
    pick_save_path_with_filter(
        default_name,
        "JSON 文件 (*.json)\0*.json\0所有文件 (*.*)\0*.*\0",
        "json",
    )
}

/// 「另存为」对话框（过滤器/默认扩展名可配；WP-B CSV 导出复用）。
/// 返回 None = 用户取消或对话框出错——两种都不写任何文件、不弹错误
/// （对话框出错留 bootlog 便于诊断）。OFN_NOCHANGEDIR 保证对话框
/// 不污染进程工作目录（IO 线程的相对路径语义不受影响）。
pub(crate) fn pick_save_path_with_filter(
    default_name: &str,
    filter: &str,
    def_ext: &str,
) -> Option<PathBuf> {
    let mut file_buf: Vec<u16> = vec![0u16; 260];
    let default_wide = wide(default_name);
    let copy_len = default_wide.len().min(file_buf.len());
    file_buf[..copy_len].copy_from_slice(&default_wide[..copy_len]);
    let filter = wide(filter);
    let def_ext = wide(def_ext);
    // windows 0.62 的 OPENFILENAMEW（x86_64 变体）不实现 Default——
    // 含裸指针字段，需 zeroed 后逐字段赋值。
    // SAFETY(E9): OPENFILENAMEW 含裸指针字段不实现 Default，zeroed 后立即逐字段赋值且先写 lStructSize；后续赋值覆盖全部被引用成员。
    let mut ofn: OPENFILENAMEW = unsafe { std::mem::zeroed() };
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.lpstrFile = PWSTR(file_buf.as_mut_ptr());
    ofn.nMaxFile = file_buf.len() as u32;
    ofn.lpstrFilter = PCWSTR(filter.as_ptr());
    ofn.nFilterIndex = 1;
    ofn.lpstrDefExt = PCWSTR(def_ext.as_ptr());
    ofn.Flags = OFN_OVERWRITEPROMPT | OFN_NOCHANGEDIR | OFN_PATHMUSTEXIST;
    // SAFETY(E9): ofn 已按契约初始化（lStructSize 正确、lpstrFile/nMaxFile 与 file_buf 容量配对）；file_buf 在调用期间存活。
    let ok = unsafe { GetSaveFileNameW(&mut ofn) };
    if !ok.as_bool() {
        // SAFETY(E9): CommDlgExtendedError 无参、返回错误码值；仅在 GetSaveFileNameW 返回失败后调用。
        let err = unsafe { CommDlgExtendedError() };
        if err.0 != 0 {
            bootlog::note(&format!(
                "save-as dialog error: commdlg extended error {}",
                err.0
            ));
        }
        return None;
    }
    let len = file_buf
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(file_buf.len());
    Some(PathBuf::from(String::from_utf16_lossy(&file_buf[..len])))
}

/// 导出落盘到用户选定路径（v0.3.15）：复用 write_diag_export 的
/// 目录自建 + 可读报错。**不做任何自动清理**——导出文件由用户自选
/// 位置、自行管理，本函数绝不删除/清理同目录的其它导出文件（含
/// %APPDATA% 下的历史导出）。
pub(crate) fn finalize_diag_export(path: &Path, json: &str) -> Result<PathBuf, String> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::path::Path::new("."));
    let filename = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("导出路径不含有效文件名：{}", path.display()))?;
    write_diag_export(dir, filename, json)
}

/// 取消路径的可测内核（v0.3.15）：None = 用户取消 → 不写任何文件、
/// 返回 None；Some(path) → 落盘并返回结果。
pub(crate) fn export_diag_to(path: Option<&Path>, json: &str) -> Option<Result<PathBuf, String>> {
    path.map(|p| finalize_diag_export(p, json))
}

/// 诊断导出（P1-6：脱敏；v0.3.12 D-69：导出重构为对象——devices +
/// log_tail + last_enum + exported_at/app_version；v0.3.15：保存位置
/// 由用户经「另存为」对话框自选，取消则不写文件、不弹错误）。
/// **导出永远成功**：任何子项失败（日志缺失/锁中毒/枚举未发生）
/// 降级为空 + reason，绝不整体报错（v0.3.2 的 os error 3 教训）。
pub(crate) fn export_diag(
    observer: &Arc<Mutex<Observer>>,
    enum_snap: &Arc<Mutex<Option<EnumSnapshot>>>,
    channel_map_miss: &Arc<AtomicU64>,
    telemetry: &Telemetry,
) {
    // devices：原事件流导出（脱敏逻辑不动）；锁中毒降级为空数组 + reason。
    // T-02a：同一次持锁内顺带收集设备名（事件流 DeviceFound；EnumSnapshot
    // 本身无设备名字段——快照侧无名可取，事件流为唯一来源；锁中毒时收集为
    // 空数组，仅 <USER> 规则生效，残余登记 T-02b 审计与对账跟踪）。
    let (devices, devices_reason, device_names) = match observer.lock() {
        Ok(o) => {
            let mut names: Vec<String> = Vec::new();
            for ev in o.events() {
                if let DiagKind::DeviceFound { name, .. } = &ev.kind {
                    if !name.is_empty() && !names.iter().any(|n| n == name) {
                        names.push(name.clone());
                    }
                }
            }
            (o.export_value(true), None, names)
        }
        Err(_) => (
            serde_json::Value::Array(Vec::new()),
            Some("observer lock poisoned".to_string()),
            Vec::new(),
        ),
    };
    // log_tail：bootlog 登记的真实日志路径（APPDATA → EXE 目录 → TEMP
    // 逐级兜底后的实际落点）；文件不存在/不可读由 read_log_tail 降级。
    let log_tail = read_log_tail(
        std::path::Path::new(&bootlog::path_str()),
        LOG_TAIL_MAX_LINES,
    );
    // last_enum：最近一次枚举快照（内存状态）；未枚举为 null。
    let (last_enum, last_enum_reason) = match enum_snap.lock() {
        Ok(g) => (g.clone(), None),
        Err(_) => (None, Some("enum snapshot lock poisoned".to_string())),
    };
    let exported_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let json = DiagExport {
        devices,
        devices_reason,
        log_tail,
        last_enum,
        last_enum_reason,
        channel_map_miss: channel_map_miss.load(Ordering::Relaxed),
        frame_tx: telemetry.frames.tx.load(Ordering::Relaxed),
        frame_rx: telemetry.frames.rx.load(Ordering::Relaxed),
        avg_tx_interval_ms: telemetry.frames.avg_tx_interval_ms(),
        feature_index_tx: telemetry.frames.feature_tx(),
        feature_index_rx: telemetry.frames.feature_rx(),
        arm_all_fills: telemetry.wake_arm_all_fills.load(Ordering::Relaxed),
        wake_successes: telemetry.wake_successes.load(Ordering::Relaxed),
        wake_miss_no_path: telemetry.wake_miss_no_path.load(Ordering::Relaxed),
        wake_miss_zero_match: telemetry.wake_miss_zero_match.load(Ordering::Relaxed),
        wake_miss_no_fingerprint: telemetry.wake_miss_no_fingerprint.load(Ordering::Relaxed),
        wake_miss_ambiguous: telemetry.wake_miss_ambiguous.load(Ordering::Relaxed),
        wake_recovery_via_reenum: telemetry.wake_recovery_via_reenum.load(Ordering::Relaxed),
        probe_cap_daily_hits: telemetry.probe_cap_daily_hits.load(Ordering::Relaxed),
        probe_backoff_skips: telemetry.probe_backoff_skips.load(Ordering::Relaxed),
        guaranteed_read_attempts: telemetry.guaranteed_read_attempts.load(Ordering::Relaxed),
        wake_success_hit_exact: telemetry.wake_success_hit_exact.load(Ordering::Relaxed),
        wake_success_hit_relaxed: telemetry.wake_success_hit_relaxed.load(Ordering::Relaxed),
        run_started_at: telemetry.run_started_at_rfc3339.clone(),
        poll_skipped_ff_cache: telemetry.poll_skipped_ff_cache.load(Ordering::Relaxed),
        poll_skipped_dead_slot: telemetry.poll_skipped_dead_slot.load(Ordering::Relaxed),
        poll_skipped_sleep: telemetry.poll_skipped_sleep.load(Ordering::Relaxed),
        poll_rounds_total: telemetry.poll_rounds_total.load(Ordering::Relaxed),
        poll_round_last_duration_ms: telemetry
            .poll_round_last_duration_ms
            .load(Ordering::Relaxed),
        poll_round_max_duration_ms: telemetry.poll_round_max_duration_ms.load(Ordering::Relaxed),
        slot_outcome_success: telemetry.slot_outcome_success.load(Ordering::Relaxed),
        slot_outcome_timeout: telemetry.slot_outcome_timeout.load(Ordering::Relaxed),
        slot_outcome_error: telemetry.slot_outcome_error.load(Ordering::Relaxed),
        slot_query_charging: telemetry.slot_query_charging.load(Ordering::Relaxed),
        slot_query_normal: telemetry.slot_query_normal.load(Ordering::Relaxed),
        manifest: volttray_app::export_manifest::manifest_json(exported_at as i64),
        exported_at,
        app_version: env!("CARGO_PKG_VERSION").to_string(),
    }
    .to_json_string_with(&device_names);
    // v0.3.15：保存位置由用户经「另存为」对话框自选；取消则不写任何
    // 文件、不弹错误，仅留 bootlog。导出文件由用户自行管理——本路径
    // 不做任何自动清理（含 %APPDATA% 下的历史导出文件）。
    // 第 7 项：默认文件名改本地时间可读格式（GetLocalTime 取时，
    // 格式化纯逻辑在 app 层跨平台可测）；JSON 内 exported_at 字段
    // 保留 unix 秒不动（程序消费）。同名碰撞由对话框
    // OFN_OVERWRITEPROMPT 覆盖前确认兜底（见 diag_export.rs 论证）。
    // SAFETY(E9): GetLocalTime 无参、返回 POD SYSTEMTIME 值，无指针契约。
    let local = unsafe { GetLocalTime() };
    let default_name = diag_export_filename_local(LocalTimeParts {
        year: local.wYear,
        month: local.wMonth,
        day: local.wDay,
        hour: local.wHour,
        minute: local.wMinute,
        second: local.wSecond,
        millis: local.wMilliseconds,
    });
    let Some(chosen) = pick_save_path(&default_name) else {
        bootlog::note("diag export cancelled by user (save-as dialog)");
        return;
    };
    let msg = match export_diag_to(Some(&chosen), &json) {
        Some(Ok(path)) => format!(
            "诊断已导出（设备事件流 + 日志尾部 {} 行 + 最近枚举快照）：\n{}",
            LOG_TAIL_MAX_LINES,
            path.display()
        ),
        Some(Err(e)) => format!("诊断导出失败：{e}"),
        None => unreachable!("Some(path) always yields Some(result)"),
    };
    let text = wide(&msg);
    let title = wide("LogiVoltTray 诊断");
    // SAFETY(E9): text/title 为 NUL 结尾宽字符缓冲，MessageBoxW 同步调用期间存活；其余参数为值类型。
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "volttray-diag-test-{}-{tag}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        p
    }

    /// #2 回归：目标目录（含多级嵌套）不存在时导出仍成功——
    /// 真机「系统找不到指定的路径。(os error 3)」的根因即缺 create_dir_all。
    #[test]
    fn export_creates_missing_nested_dirs() {
        let root = temp_root("nested");
        let dir = root.join("a").join("b").join("c");
        let path = write_diag_export(&dir, "LogiVoltTray-diag-export-1.json", "[]")
            .expect("export should succeed on missing dirs");
        assert!(path.is_file());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[]",
            "exported content must match"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
    /// #2：导出目录被同名文件挡住时，错误可读且含原始错误。
    #[test]
    fn export_blocked_by_file_gives_readable_error() {
        let root = temp_root("blocked");
        std::fs::create_dir_all(&root).unwrap();
        let blocker = root.join("blocker");
        std::fs::write(&blocker, b"i am a file").unwrap();
        let err = write_diag_export(&blocker, "x.json", "[]")
            .expect_err("export into a file path must fail");
        assert!(
            err.contains("无法创建导出目录"),
            "readable error expected, got: {err}"
        );
        assert!(err.contains("原始错误"), "raw error retained: {err}");
        let _ = std::fs::remove_dir_all(&root);
    }
    /// #2：导出文件名统一为 LogiVoltTray- 前缀（#1 命名一致性）。
    #[test]
    fn export_filename_uses_logivolttray_prefix() {
        let root = temp_root("prefix");
        let path = write_diag_export(&root, "LogiVoltTray-diag-export-42.json", "{}").unwrap();
        assert!(path
            .to_string_lossy()
            .contains("LogiVoltTray-diag-export-42.json"));
        let _ = std::fs::remove_dir_all(&root);
    }
    /// v0.3.15：用户在「另存为」对话框取消 → 不写任何文件、不报错
    /// （None 路径的可测内核）；Some 路径正常落盘。
    #[test]
    fn cancel_path_writes_nothing() {
        let root = temp_root("cancel");
        let target = root.join("sub").join("out.json");
        assert!(
            export_diag_to(None, "{}").is_none(),
            "cancel must not attempt any write"
        );
        assert!(!root.exists(), "cancel must not create the target dir/file");
        let written = export_diag_to(Some(&target), "{}")
            .expect("Some(path) yields a result")
            .expect("write to chosen path succeeds");
        assert!(written.is_file());
        let _ = std::fs::remove_dir_all(&root);
    }
    /// v0.3.15（19:17 用户拍板）：导出成功路径**不删除任何已有导出
    /// 文件**——同目录的历史导出与无关文件逐字节保持原样（软件不做
    /// 任何形式的自动清理，文件由用户自行管理）。
    #[test]
    fn save_success_leaves_sibling_exports_untouched() {
        let root = temp_root("siblings");
        std::fs::create_dir_all(&root).unwrap();
        let old1 = root.join("LogiVoltTray-diag-export-1.json");
        let old2 = root.join("LogiVoltTray-diag-export-2.json");
        let unrelated = root.join("unrelated.txt");
        std::fs::write(&old1, b"{\"old\":1}").unwrap();
        std::fs::write(&old2, b"{\"old\":2}").unwrap();
        std::fs::write(&unrelated, b"keep me").unwrap();
        let target = root.join("LogiVoltTray-diag-export-3.json");
        let written = finalize_diag_export(&target, "{}").expect("export succeeds");
        assert!(written.is_file());
        assert_eq!(std::fs::read(&old1).unwrap(), b"{\"old\":1}");
        assert_eq!(std::fs::read(&old2).unwrap(), b"{\"old\":2}");
        assert_eq!(std::fs::read(&unrelated).unwrap(), b"keep me");
        let _ = std::fs::remove_dir_all(&root);
    }
}
