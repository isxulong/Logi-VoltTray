//! Logi VoltTray 托盘主程序（仅 Windows 提供完整功能）。
//!
//! 线程模型：GUI 线程（托盘 + 消息泵）+ IO 线程（HID 枚举 / 握手 / 轮询）。
//! 跨线程只走 channel 与共享的 AppCore / Observer（诊断观测流 P1-2）。
//!
//! v0.2.0（红队差异 #1 决定性修正）：不再对 0xFF 一把梭——按配对通道枚举
//! （`enumerate_channels`），每个接收器通道逐槽扫描 1..=6 再试 0xFF（参考
//! 实现 `_scan_channel` 实证），拿到电量才注册设备；读不到时原因分类已进
//! 诊断流（`BatteryUnknown`），不再留下无法定位的常驻「？」空设备。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(windows)]
mod icon;
#[cfg(windows)]
mod menu;
#[cfg(windows)]
mod single;
#[cfg(windows)]
mod toast;

#[cfg(windows)]
mod imp {
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex, RwLock};
    use std::time::{Duration, Instant};

    use tray_icon::menu::MenuEvent;
    use tray_icon::{TrayIconBuilder, TrayIconEvent};
    use windows::core::PCWSTR;
    use windows::Win32::UI::HiDpi::{
        GetDpiForSystem, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    use windows::Win32::UI::WindowsAndMessaging::*;

    use volttray_app::aumid;
    use volttray_app::autostart;
    use volttray_app::config::{IconBackground, IconFontColor};
    use volttray_app::{Alert, AppCore, Config, ConfigWatcher, Observer, SharedObserver, Snapshot};
    use volttray_device::battery::probe_battery;
    use volttray_device::handshake::{handshake, Handshake};
    use volttray_device::{classify_pid, Device};
    use volttray_protocol::FeatureTable;
    use volttray_transport::channel::{HidChannel, IoError};
    use volttray_transport::events::{DiagEvent, DiagKind, Layer};
    use volttray_transport::hid_impl::{enumerate_channels, ChannelInfo, HidDeviceChannel};
    use volttray_transport::Transport;

    use crate::icon::{dpi_bucket_size, icon_text, system_dark_theme, IconCache, IconColor};
    use crate::menu::build_menu;
    use crate::single::{acquire, SingleInstance};
    use crate::toast;

    /// 接收器槽位扫描顺序（参考实现 `_scan_channel` 实证：1..=6 再 0xFF）。
    const SCAN_SLOTS: [u8; 7] = [1, 2, 3, 4, 5, 6, 0xFF];
    /// 电压曲线名（v0.2.0：参考实现 13 锚点实证曲线）。
    const CURVE: &str = "li-ion-logi";

    enum GuiMsg {
        Snapshot(Box<Snapshot>),
        Alert,
    }

    enum IoCmd {
        RefreshNow,
        Shutdown,
    }

    /// 同一接收器通道的句柄共享：多个槽位（设备）共用一对 HID collection。
    #[derive(Clone)]
    struct SharedChannel(Arc<Mutex<HidDeviceChannel>>);

    impl HidChannel for SharedChannel {
        fn write(&mut self, d: &[u8]) -> Result<usize, IoError> {
            self.0
                .lock()
                .map_err(|_| IoError::Other("channel mutex poisoned".into()))?
                .write(d)
        }
        fn read(&mut self, buf: &mut [u8], t: Duration) -> Result<usize, IoError> {
            self.0
                .lock()
                .map_err(|_| IoError::Other("channel mutex poisoned".into()))?
                .read(buf, t)
        }
        fn drain(&mut self) {
            if let Ok(mut c) = self.0.lock() {
                c.drain();
            }
        }
        fn get_feature_report(&mut self, id: u8, buf: &mut [u8]) -> Result<usize, IoError> {
            self.0
                .lock()
                .map_err(|_| IoError::Other("channel mutex poisoned".into()))?
                .get_feature_report(id, buf)
        }
    }

    struct SlotState {
        id: String,
        index: u8,
        features: FeatureTable,
        is_hidpp1: bool,
        failures: u32,
    }

    struct ChannelState {
        key: String,
        info: ChannelInfo,
        shared: SharedChannel,
        slots: Vec<SlotState>,
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// 平面 TOML 的布尔键就地改写（配置所有权在 app，托盘只触发持久化）。
    fn persist_bool(path: &Path, key: &str, value: bool) {
        if let Ok(text) = std::fs::read_to_string(path) {
            let mut out = String::with_capacity(text.len() + 16);
            let mut done = false;
            for line in text.lines() {
                if !done && line.trim_start().starts_with(key) {
                    out.push_str(&format!("{key} = {value}"));
                    done = true;
                } else {
                    out.push_str(line);
                }
                out.push('\n');
            }
            let _ = std::fs::write(path, out);
        }
    }

    fn alert_text(alert: Alert, name: &str) -> (String, String) {
        match alert {
            Alert::Warn(p) => ("VoltTray 电量偏低".into(), format!("{name} 电量剩余 {p}%")),
            Alert::Critical(p) => (
                "VoltTray 电量严重不足".into(),
                format!("{name} 电量仅剩 {p}%，请尽快充电"),
            ),
        }
    }

    /// 槽位显示名：接收器产品名 + 槽位号（接收器本体 0xFF 不加后缀）。
    fn slot_name(info: &ChannelInfo, slot: u8) -> String {
        let base = if info.product.is_empty() {
            "Logitech 设备".to_string()
        } else {
            info.product.clone()
        };
        if slot == 0xFF {
            base
        } else {
            format!("{base} ·{slot}")
        }
    }

    /// 对单个通道扫描一个槽位：握手 → 电量探测；拿到电量才注册。
    /// 返回注册成功的槽位（None = 未发现/电量未知，原因已进诊断流）。
    fn try_slot(
        core: &Arc<Mutex<AppCore>>,
        observer: &Arc<Mutex<Observer>>,
        shared: &SharedChannel,
        info: &ChannelInfo,
        slot: u8,
    ) -> Option<SlotState> {
        let id = format!("{}#{}", info.key(), slot);
        let mut t = Transport::new(shared.clone(), SharedObserver(observer.clone()));
        let hs = handshake(&mut t, slot);
        let (features, is_hidpp1) = match hs {
            Handshake::Hidpp2(table) => (table, false),
            Handshake::Hidpp1 => (FeatureTable::new(), true),
            Handshake::NoResponse => return None,
        };
        // 电量探测：失败不注册（BatteryUnknown 事件已由 probe_battery 记录原因分类）。
        let reading = probe_battery(&mut t, &id, slot, &features, CURVE, is_hidpp1).ok()?;
        let name = slot_name(info, slot);
        let dev = Device::new(id.clone(), &name, classify_pid(info.pid), slot, is_hidpp1);
        let now = Instant::now();
        if let Ok(mut c) = core.lock() {
            c.upsert_device(dev);
            c.record_success(&id, reading, now);
        }
        if let Ok(mut o) = observer.lock() {
            o.push(DiagEvent::new(
                Layer::App,
                DiagKind::DeviceFound {
                    id: id.clone(),
                    name,
                },
            ));
        }
        Some(SlotState {
            id,
            index: slot,
            features,
            is_hidpp1,
            failures: 0,
        })
    }

    /// IO 线程：枚举 → 握手 → 周期轮询。真机行为未验证（见 docs/decisions.md）。
    fn io_loop(
        core: Arc<Mutex<AppCore>>,
        observer: Arc<Mutex<Observer>>,
        gui: mpsc::Sender<GuiMsg>,
        cmd: mpsc::Receiver<IoCmd>,
        cfg_path: PathBuf,
    ) {
        let mut channels: Vec<ChannelState> = Vec::new();
        let mut watcher = ConfigWatcher::new(cfg_path);
        let mut last_enum = Instant::now() - Duration::from_secs(60);
        'outer: loop {
            if matches!(cmd.try_recv(), Ok(IoCmd::Shutdown)) {
                return;
            }

            // 30s 重新枚举一次（或当前无通道时每次循环都枚举）。
            if last_enum.elapsed() >= Duration::from_secs(30) || channels.is_empty() {
                last_enum = Instant::now();
                if let Ok(api) = hidapi::HidApi::new() {
                    let mut seen: Vec<String> = Vec::new();
                    for info in enumerate_channels(&api) {
                        let key = info.key();
                        seen.push(key.clone());
                        if let Some(state) = channels.iter_mut().find(|c| c.key == key) {
                            // 已有通道：补扫新出现的槽位（休眠唤醒 / 新配对）。
                            for slot in SCAN_SLOTS {
                                if state.slots.iter().any(|s| s.index == slot) {
                                    continue;
                                }
                                if let Some(s) =
                                    try_slot(&core, &observer, &state.shared, &state.info, slot)
                                {
                                    state.slots.push(s);
                                }
                            }
                            continue;
                        }
                        let Ok(ch) = HidDeviceChannel::open(
                            &api,
                            info.short_path.as_deref(),
                            info.long_path.as_deref(),
                        ) else {
                            continue;
                        };
                        let shared = SharedChannel(Arc::new(Mutex::new(ch)));
                        let mut slots = Vec::new();
                        for slot in SCAN_SLOTS {
                            if let Some(s) = try_slot(&core, &observer, &shared, &info, slot) {
                                slots.push(s);
                            }
                        }
                        channels.push(ChannelState {
                            key,
                            info,
                            shared,
                            slots,
                        });
                    }
                    // 掉线通道整体移除（接收器拔出）。
                    let mut i = 0;
                    while i < channels.len() {
                        if !seen.contains(&channels[i].key) {
                            let gone = channels.remove(i);
                            for s in &gone.slots {
                                if let Ok(mut c) = core.lock() {
                                    c.remove_device(&s.id);
                                }
                                if let Ok(mut o) = observer.lock() {
                                    o.push(DiagEvent::new(
                                        Layer::App,
                                        DiagKind::DeviceLost { id: s.id.clone() },
                                    ));
                                }
                            }
                        } else {
                            i += 1;
                        }
                    }
                }
            }

            // 轮询一轮：逐通道逐槽位探测电量。
            let now = Instant::now();
            let mut alerts: Vec<(String, String)> = Vec::new();
            {
                let mut core = match core.lock() {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                for ch_state in &mut channels {
                    let mut i = 0;
                    while i < ch_state.slots.len() {
                        let s = &mut ch_state.slots[i];
                        let mut t = Transport::new(
                            ch_state.shared.clone(),
                            SharedObserver(observer.clone()),
                        );
                        let reading = probe_battery(
                            &mut t,
                            &s.id,
                            s.index,
                            &s.features,
                            CURVE,
                            s.is_hidpp1,
                        );
                        match reading {
                            Ok(r) => {
                                s.failures = 0;
                                if let Some(a) = core.record_success(&s.id, r, now) {
                                    let name = core
                                        .entries()
                                        .iter()
                                        .find(|e| e.device.id == s.id)
                                        .map(|e| e.device.name.clone())
                                        .unwrap_or_else(|| "设备".into());
                                    alerts.push(alert_text(a, &name));
                                }
                                i += 1;
                            }
                            Err(_) => {
                                s.failures += 1;
                                let gone = s.failures >= 3;
                                core.record_failure(&s.id, gone);
                                if gone {
                                    let gone_slot = ch_state.slots.remove(i);
                                    if let Ok(mut o) = observer.lock() {
                                        o.push(DiagEvent::new(
                                            Layer::App,
                                            DiagKind::DeviceLost {
                                                id: gone_slot.id,
                                            },
                                        ));
                                    }
                                } else {
                                    i += 1;
                                }
                            }
                        }
                    }
                }
                for id in core.newly_gray(now) {
                    alerts.push((
                        "VoltTray 设备离线".into(),
                        format!("设备 {id} 超过 90 秒未响应，已置灰"),
                    ));
                }
                let snap = core.snapshot(now);
                let _ = gui.send(GuiMsg::Snapshot(Box::new(snap)));
            }
            let (notify_enabled, silent) = {
                match core.lock() {
                    Ok(c) => {
                        let cfg = c.config();
                        (cfg.notifications_enabled, cfg.notification_silent)
                    }
                    Err(_) => (true, false),
                }
            };
            for (title, body) in alerts {
                if notify_enabled {
                    let _ = toast::notify(&title, &body, silent);
                }
                let _ = gui.send(GuiMsg::Alert);
            }

            // 配置热重载（P1-2：所有权在 app 层）。
            if let Some(Ok((cfg, _warnings))) = watcher.poll() {
                if let Ok(c) = core.lock() {
                    c.swap_config(cfg);
                }
                if let Ok(mut o) = observer.lock() {
                    o.push(DiagEvent::new(
                        Layer::App,
                        DiagKind::Info {
                            message: "config reloaded".into(),
                        },
                    ));
                }
            }

            // 分片睡眠，响应立即刷新 / 退出。
            let interval = match core.lock() {
                Ok(c) => c.poll_interval(),
                Err(_) => Duration::from_secs(60),
            };
            let deadline = Instant::now() + interval;
            while Instant::now() < deadline {
                match cmd.recv_timeout(Duration::from_millis(200)) {
                    Ok(IoCmd::RefreshNow) => continue 'outer,
                    Ok(IoCmd::Shutdown) => return,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
        }
    }

    struct GuiState {
        snapshot: Option<Snapshot>,
        pinned: Option<String>,
        autostart_on: bool,
        silent: bool,
        icon_bg: IconBackground,
        icon_font: IconFontColor,
    }

    pub fn run() -> i32 {
        // 1. 单实例（P1-3）：第二实例发激活事件后退出。
        let _mutex = match acquire() {
            Ok(SingleInstance::First(h)) => h,
            Ok(SingleInstance::Second) => return 0,
            Err(e) => {
                eprintln!("single instance: {e}");
                return 1;
            }
        };

        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }

        // 2. AUMID + 开始菜单快捷方式（P0-5，尽力而为）。
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("volttray-tray.exe"));
        let _ = volttray_app::guarded("aumid", || {
            let _ = aumid::set_process_aumid();
        });
        let _ = volttray_app::guarded("shortcut", || {
            let _ = aumid::ensure_start_menu_shortcut(&exe);
        });

        // 3. 配置：不存在则按 schema 默认值创建（P1-8）。
        let cfg_path = Config::default_path();
        let cfg = match Config::load(&cfg_path) {
            Ok((c, _w)) => c,
            Err(_) => {
                let c = Config::default();
                if let Some(parent) = cfg_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&cfg_path, c.to_toml());
                c
            }
        };

        let config_arc = Arc::new(RwLock::new(cfg.clone()));
        let observer = Arc::new(Mutex::new(Observer::new()));
        let core = Arc::new(Mutex::new(AppCore::new(config_arc)));

        let (gui_tx, gui_rx) = mpsc::channel::<GuiMsg>();
        let (cmd_tx, cmd_rx) = mpsc::channel::<IoCmd>();
        {
            let core = core.clone();
            let observer = observer.clone();
            let path = cfg_path.clone();
            std::thread::spawn(move || io_loop(core, observer, gui_tx, cmd_rx, path));
        }

        // 4. 图标缓存（评审 4.1：启动预渲染；v0.2.0：含背景/字色设置）+ 托盘。
        let dpi = unsafe { GetDpiForSystem() };
        let dark = system_dark_theme();
        let mut cache = IconCache::new(
            dpi_bucket_size(dpi),
            dark,
            cfg.icon_background,
            cfg.icon_font_color,
        );
        let mut cur_size = dpi_bucket_size(dpi);
        let mut cur_dark = dark;

        let mut state = GuiState {
            snapshot: None,
            pinned: None,
            autostart_on: autostart::is_enabled(),
            silent: cfg.notification_silent,
            icon_bg: cfg.icon_background,
            icon_font: cfg.icon_font_color,
        };

        let initial = icon_text(None, false);
        let Some(first_icon) = cache.icon_for(IconColor::Default, &initial) else {
            eprintln!("icon cache init failed");
            return 1;
        };
        let (menu_obj, mut ids) = build_menu(&[], None, None, state.autostart_on, state.silent);
        let tray = match TrayIconBuilder::new()
            .with_menu(Box::new(menu_obj))
            .with_tooltip("Logi VoltTray")
            .with_icon(first_icon)
            .with_menu_on_left_click(false)
            .build()
        {
            Ok(t) => t,
            Err(e) => {
                eprintln!("tray init: {e}");
                return 1;
            }
        };

        unsafe {
            let _ = SetTimer(None, 1, 500, None);
        }

        // 5. 消息泵：500ms 定时器驱动事件归并。
        let mut msg = MSG::default();
        unsafe {
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                if msg.message == WM_TIMER {
                    pump(
                        &tray,
                        &gui_rx,
                        &cmd_tx,
                        &mut state,
                        &mut cache,
                        &mut cur_size,
                        &mut cur_dark,
                        &core,
                        &observer,
                        &cfg_path,
                        &exe,
                        &mut ids,
                    );
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let _ = KillTimer(None, 1);
        }
        0
    }

    #[allow(clippy::too_many_arguments)]
    fn pump(
        tray: &tray_icon::TrayIcon,
        gui_rx: &mpsc::Receiver<GuiMsg>,
        cmd_tx: &mpsc::Sender<IoCmd>,
        state: &mut GuiState,
        cache: &mut IconCache,
        cur_size: &mut u32,
        cur_dark: &mut bool,
        core: &Arc<Mutex<AppCore>>,
        observer: &Arc<Mutex<Observer>>,
        cfg_path: &Path,
        exe: &Path,
        ids: &mut crate::menu::MenuIds,
    ) {
        // 应用层消息。
        let mut snap_changed = false;
        while let Ok(m) = gui_rx.try_recv() {
            match m {
                GuiMsg::Snapshot(s) => {
                    state.snapshot = Some(*s);
                    snap_changed = true;
                }
                GuiMsg::Alert => {} // toast 已在 IO 线程发出；此处仅占位便于扩展。
            }
        }

        // DPI / 主题 / 图标背景 / 字色任一变化 → 重建图标缓存
        //（评审 4.1 + v0.2.0：设置变更经热重载即时生效）。
        let (bg, font) = match core.lock() {
            Ok(c) => {
                let cfg = c.config();
                (cfg.icon_background, cfg.icon_font_color)
            }
            Err(_) => (state.icon_bg, state.icon_font),
        };
        if bg != state.icon_bg || font != state.icon_font {
            state.icon_bg = bg;
            state.icon_font = font;
            snap_changed = true;
        }
        let dpi = unsafe { GetDpiForSystem() };
        let size = dpi_bucket_size(dpi);
        let dark = system_dark_theme();
        if size != *cur_size || dark != *cur_dark {
            cache.rebuild(size, dark, state.icon_bg, state.icon_font);
            *cur_size = size;
            *cur_dark = dark;
            snap_changed = true;
        } else if snap_changed {
            cache.rebuild(size, dark, state.icon_bg, state.icon_font);
        }

        if snap_changed {
            refresh_ui(tray, state, cache, ids);
        }

        // 托盘图标事件（左键上弹由菜单接管，其余暂不处理）。
        while let Ok(_e) = TrayIconEvent::receiver().try_recv() {}

        // 菜单事件。
        while let Ok(e) = MenuEvent::receiver().try_recv() {
            if e.id == ids.refresh {
                let _ = cmd_tx.send(IoCmd::RefreshNow);
            } else if e.id == ids.diag {
                export_diag(observer);
            } else if e.id == ids.autostart {
                let now_on = if state.autostart_on {
                    autostart::disable().is_ok()
                } else {
                    autostart::enable(exe).is_ok()
                };
                state.autostart_on = if state.autostart_on { !now_on } else { now_on };
                persist_bool(cfg_path, "autostart", state.autostart_on);
                refresh_ui(tray, state, cache, ids);
            } else if e.id == ids.silent {
                state.silent = !state.silent;
                persist_bool(cfg_path, "notification_silent", state.silent);
                refresh_ui(tray, state, cache, ids);
            } else if e.id == ids.exit {
                let _ = cmd_tx.send(IoCmd::Shutdown);
                unsafe { PostQuitMessage(0) };
            } else if let Some((_, dev_id)) = ids.device_rows.iter().find(|(mid, _)| *mid == e.id) {
                state.pinned = Some(dev_id.clone());
                refresh_ui(tray, state, cache, ids);
            }
        }
    }

    fn refresh_ui(
        tray: &tray_icon::TrayIcon,
        state: &mut GuiState,
        cache: &IconCache,
        ids: &mut crate::menu::MenuIds,
    ) {
        let (devices, default_idx) = match &state.snapshot {
            Some(s) => (s.devices.clone(), s.default_device),
            None => (Vec::new(), None),
        };
        let default_id = default_idx
            .and_then(|i| devices.get(i))
            .map(|d| d.id.clone());
        let (menu_obj, new_ids) = build_menu(
            &devices,
            default_id.as_deref(),
            state.pinned.as_deref(),
            state.autostart_on,
            state.silent,
        );
        *ids = new_ids;
        tray.set_menu(Some(Box::new(menu_obj)));

        // 图标：默认展示设备（或钉选设备）的电量。
        let show = state
            .pinned
            .as_ref()
            .and_then(|p| devices.iter().find(|d| &d.id == p))
            .or_else(|| default_idx.and_then(|i| devices.get(i)));
        let (percent, gray) = match show {
            Some(d) => (d.percent, d.gray),
            None => (None, false),
        };
        let text = icon_text(percent, gray);
        let color = match percent {
            Some(p) => IconColor::for_percent(p, gray),
            None => IconColor::Default,
        };
        if let Some(ic) = cache.icon_for(color, &text) {
            let _ = tray.set_icon(Some(ic));
        }
        let tip = match show {
            Some(d) => format!("Logi VoltTray — {} {}", d.name, text),
            None => "Logi VoltTray".to_string(),
        };
        let _ = tray.set_tooltip(Some(&tip));
    }

    /// 诊断导出（P1-6：脱敏 JSON 落盘 + 弹窗告知路径）。
    fn export_diag(observer: &Arc<Mutex<Observer>>) {
        let json = match observer.lock() {
            Ok(o) => o.export_json(true),
            Err(_) => "[]".to_string(),
        };
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let path = Config::default_path()
            .parent()
            .map(|p| p.join(format!("diag-export-{secs}.json")))
            .unwrap_or_else(|| PathBuf::from(format!("diag-export-{secs}.json")));
        let msg = match std::fs::write(&path, json) {
            Ok(()) => format!("诊断事件流已脱敏导出：\n{}", path.display()),
            Err(e) => format!("导出失败：{e}"),
        };
        let text = wide(&msg);
        let title = wide("Logi VoltTray 诊断");
        unsafe {
            MessageBoxW(
                None,
                PCWSTR(text.as_ptr()),
                PCWSTR(title.as_ptr()),
                MB_OK | MB_ICONINFORMATION,
            );
        }
    }
}

fn main() {
    #[cfg(windows)]
    {
        let code = std::panic::catch_unwind(imp::run).unwrap_or(3);
        std::process::exit(code);
    }
    #[cfg(not(windows))]
    {
        eprintln!("Logi VoltTray 仅支持 Windows；此二进制在非 Windows 平台仅用于编译与单元测试。");
        std::process::exit(2);
    }
}
