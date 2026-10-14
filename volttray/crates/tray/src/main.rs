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
mod blink;
#[cfg(windows)]
mod bootlog;
#[cfg(windows)]
mod icon;
#[cfg(windows)]
mod menu;
#[cfg(windows)]
mod rawinput;
#[cfg(windows)]
mod settings;
#[cfg(windows)]
mod single;
#[cfg(windows)]
mod toast;

/// v0.3.8（D-64/B-2）：启动时图标渲染回退出厂默认的原因；首次打开设置
/// 窗口时提示一次后取走（None = 未发生回退或已提示过）。
#[cfg(windows)]
static APPEARANCE_FALLBACK_NOTE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

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
    use volttray_app::config::{
        font_tier_for_percent, IconBackground, IconFontColor, IconShape, RgbColor,
    };
    use volttray_app::{
        Alert, AppCore, Config, ConfigWatcher, Observer, PollDecision, SharedObserver, Snapshot,
        TimeoutOutcome,
    };
    use volttray_device::battery::{probe_battery, UnknownReason};
    use volttray_device::handshake::{handshake, Handshake};
    use volttray_device::name::read_device_name;
    use volttray_device::state::DeviceState;
    use volttray_device::{classify_pid, Device};
    use volttray_protocol::FeatureTable;
    use volttray_transport::channel::{HidChannel, IoError};
    use volttray_transport::events::{DiagEvent, DiagKind, Layer};
    use volttray_transport::hid_impl::{enumerate_channels, ChannelInfo, HidDeviceChannel};
    use volttray_transport::Transport;

    use crate::bootlog;
    use crate::icon::{
        cache_from_config, dpi_bucket_size, icon_text, startup_frame_color, system_dark_theme,
        IconCache, IconColor,
    };
    use crate::menu::build_menu;
    use crate::rawinput;
    use crate::single::{acquire, SingleInstance};
    use crate::toast;
    #[cfg(windows)]
    use crate::APPEARANCE_FALLBACK_NOTE;

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
        /// 被动唤醒信号（D-33）：系统层观测到罗技鼠标活动（Raw Input）。
        /// 仅装填一次唤醒探测资格，不直接发查询。
        WakeSignal,
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
        /// 非超时类硬失败连击（v0.3.1 语义：查询超时走休眠路径、不注销；
        /// 仅 I/O / 映射类持续失败沿用 3 连败注销，注销后靠重枚举重新注册）。
        hard_failures: u32,
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

    /// 平面 TOML 的标量键就地改写；键不存在则追加到文件尾（配置所有权在 app，
    /// 托盘/设置窗口只触发持久化）。读取失败（如文件不存在）则只写该键。
    fn persist_kv(path: &Path, key: &str, value: &str) {
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
            if !done {
                out.push_str(&format!("{key} = {value}\n"));
            }
            let _ = std::fs::write(path, out);
        } else {
            let _ = std::fs::write(path, format!("{key} = {value}\n"));
        }
    }

    fn persist_bool(path: &Path, key: &str, value: bool) {
        persist_kv(path, key, if value { "true" } else { "false" });
    }

    fn alert_text(alert: Alert, name: &str) -> (String, String) {
        match alert {
            Alert::LowBattery(p) => (
                "LogiVoltTray 电量偏低".into(),
                format!("{name} 电量剩余 {p}%，请及时充电"),
            ),
        }
    }

    /// 槽位显示名（v0.3.0「?」修复：消毒白名单已含 U+00B7「·」）：
    /// 真实设备名取不到时的干净回退，分隔符用「·」+ 空格，杜绝裸「?」。
    fn slot_name(info: &ChannelInfo, slot: u8) -> String {
        let base = if info.product.is_empty() {
            "Logitech 设备".to_string()
        } else {
            info.product.clone()
        };
        if slot == 0xFF {
            base
        } else {
            format!("{base} · 槽位 {slot}")
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
        // v0.3.0「?」修复：优先取真实设备名（HID++ DEVICE_NAME / 接收器配对名
        // 寄存器 0x2B5，见 name.rs 来源链）；取不到用干净回退，不出裸「?」。
        let raw_name = read_device_name(&mut t, slot, &features, is_hidpp1)
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| slot_name(info, slot));
        let dev = Device::new(
            id.clone(),
            &raw_name,
            classify_pid(info.pid),
            slot,
            is_hidpp1,
        );
        let name = dev.name.clone();
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
            hard_failures: 0,
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
        // v0.3.6（D-62）：热重载告警去重——同一组迁移告警只上报一次。
        let mut last_reload_warnings: Vec<String> = Vec::new();
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

            // 轮询一轮：逐通道逐槽位探测电量（D-33：休眠槽位一律 Skip——
            // 零主动查询；唯一裁决点 AppCore::poll_decision，app 层单测覆盖）。
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
                        // 休眠且无唤醒资格：本轮绝不向该设备发任何 HID++ 查询。
                        if core.poll_decision(&s.id, now) == PollDecision::Skip {
                            i += 1;
                            continue;
                        }
                        // 唤醒探测开始：消耗唤醒资格并进入冷却（D-33）。
                        let sleeping = core.entries().iter().any(|e| {
                            e.device.id == s.id && e.device.state == DeviceState::Sleeping
                        });
                        if sleeping {
                            core.begin_wake_probe(&s.id, now);
                        }
                        let mut t = Transport::new(
                            ch_state.shared.clone(),
                            SharedObserver(observer.clone()),
                        );
                        let reading =
                            probe_battery(&mut t, &s.id, s.index, &s.features, CURVE, s.is_hidpp1);
                        match reading {
                            Ok(r) => {
                                s.hard_failures = 0;
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
                            Err(reason) => match reason {
                                // 查询超时 = 疑似休眠：连击 2 次暂停轮询（不注销、
                                // 不重试——对休眠中的鼠标发查询才是真费电，D-33）。
                                UnknownReason::QueryTimeout => {
                                    if core.record_timeout(&s.id) == TimeoutOutcome::EnteredSleep {
                                        if let Ok(mut o) = observer.lock() {
                                            o.push(DiagEvent::new(
                                                Layer::App,
                                                DiagKind::StateChange {
                                                    id: s.id.clone(),
                                                    from: "online".into(),
                                                    to: "sleeping".into(),
                                                },
                                            ));
                                        }
                                    }
                                    i += 1;
                                }
                                // 接收器报告设备消失：立即注销（不等 3 次）。
                                UnknownReason::DeviceGone => {
                                    core.record_failure(&s.id, true);
                                    let gone_slot = ch_state.slots.remove(i);
                                    if let Ok(mut o) = observer.lock() {
                                        o.push(DiagEvent::new(
                                            Layer::App,
                                            DiagKind::DeviceLost { id: gone_slot.id },
                                        ));
                                    }
                                }
                                // 其它持续失败（I/O / 映射 / 特性缺失等）：沿用
                                // 3 连败注销，靠重枚举重新注册（原 v0.2.0 语义）。
                                _ => {
                                    s.hard_failures += 1;
                                    let gone = s.hard_failures >= 3;
                                    core.record_failure(&s.id, gone);
                                    if gone {
                                        let gone_slot = ch_state.slots.remove(i);
                                        if let Ok(mut o) = observer.lock() {
                                            o.push(DiagEvent::new(
                                                Layer::App,
                                                DiagKind::DeviceLost { id: gone_slot.id },
                                            ));
                                        }
                                    } else {
                                        i += 1;
                                    }
                                }
                            },
                        }
                    }
                }
                for id in core.newly_gray(now) {
                    alerts.push((
                        "LogiVoltTray 设备离线".into(),
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
            if let Some(Ok((cfg, warnings))) = watcher.poll() {
                if let Ok(c) = core.lock() {
                    c.swap_config(cfg);
                }
                if let Ok(mut o) = observer.lock() {
                    // P2-1：钳制/未知键警告不再静默丢弃（M9 AC3）——
                    // 有警告升级为 config_reloaded 事件，无警告维持 info 级。
                    // v0.3.6（D-62）：迁移告警幂等但每次加载都重复出现，
                    // 同一组告警只上报一次，不随每次热重载重复刷屏。
                    let ws: Vec<String> = warnings.iter().map(|w| w.to_string()).collect();
                    let kind = if ws.is_empty() {
                        Some(DiagKind::Info {
                            message: "config reloaded".into(),
                        })
                    } else if ws != last_reload_warnings {
                        Some(DiagKind::ConfigReloaded {
                            warnings: ws.clone(),
                        })
                    } else {
                        None
                    };
                    last_reload_warnings = ws;
                    if let Some(kind) = kind {
                        o.push(DiagEvent::new(Layer::App, kind));
                    }
                }
            }

            // 分片睡眠，响应立即刷新 / 唤醒信号 / 退出。
            let interval = match core.lock() {
                Ok(c) => c.poll_interval(),
                Err(_) => Duration::from_secs(60),
            };
            let deadline = Instant::now() + interval;
            while Instant::now() < deadline {
                match cmd.recv_timeout(Duration::from_millis(200)) {
                    Ok(IoCmd::RefreshNow) => continue 'outer,
                    // 被动唤醒（D-33）：装填一次唤醒探测资格并立即跑一轮——
                    // 恢复延迟取决于用户动鼠标的那一刻，而不是轮询周期。
                    Ok(IoCmd::WakeSignal) => {
                        if let Ok(mut c) = core.lock() {
                            for id in c.sleeping_ids() {
                                c.note_wake_signal(&id);
                            }
                        }
                        continue 'outer;
                    }
                    Ok(IoCmd::Shutdown) => return,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
        }
    }

    /// GUI 侧缓存的可视化配置（与共享配置逐 tick 对账，变化即重建图标缓存）。
    struct GuiState {
        snapshot: Option<Snapshot>,
        pinned: Option<String>,
        autostart_on: bool,
        silent: bool,
        /// 以下均为共享配置的镜像（设置窗口 / TOML 改动经 core 同步进来）。
        icon_shape: IconShape,
        icon_bg: IconBackground,
        icon_font: IconFontColor,
        icon_family: String,
        poll_secs: u64,
        blink_enabled: bool,
        low_threshold: u8,
        /// 分档着色镜像（v0.3.3 #4）：阈值数组 + 颜色数组，热重载生效。
        tiers_enabled: bool,
        tier_thresholds: Vec<u8>,
        tier_colors: Vec<RgbColor>,
        /// 菜单内容签名（v0.3.3 卫生项：P3-1 右键菜单闪烁缓解——内容未变
        /// 不重建菜单，避免菜单打开时反复 set_menu 引发的闪烁）。
        menu_sig: String,
        /// 低电闪烁状态机（M11；v0.3.4 #3 抽成 blink 模块，见 blink.rs）。
        blink: crate::blink::BlinkState,
        /// 图标文本/背景缩放镜像（v0.3.4 #4，D-48）。
        icon_text_scale: u8,
        icon_bg_scale: u8,
    }

    impl GuiState {
        fn from_config(cfg: &Config) -> Self {
            GuiState {
                snapshot: None,
                pinned: None,
                autostart_on: autostart::is_enabled(),
                silent: cfg.notification_silent,
                icon_shape: cfg.icon_shape,
                icon_bg: cfg.icon_background,
                icon_font: cfg.icon_font_color,
                icon_family: cfg.icon_font_family.clone(),
                poll_secs: cfg.poll_interval_secs,
                blink_enabled: cfg.blink_enabled,
                low_threshold: cfg.low_threshold,
                tiers_enabled: cfg.icon_font_tiers_enabled,
                tier_thresholds: cfg.icon_font_tier_thresholds.clone(),
                tier_colors: cfg.icon_font_tier_colors.clone(),
                menu_sig: String::new(),
                blink: crate::blink::BlinkState::new(),
                icon_text_scale: cfg.icon_text_scale,
                icon_bg_scale: cfg.icon_bg_scale,
            }
        }
    }

    pub fn run() -> i32 {
        // v0.3.7（D-63）：启动可见化——最先初始化崩溃日志 + panic hook，
        // 此后任何启动阶段失败都会留痕并弹中文错误，不再「双击没反应」。
        bootlog::init();
        bootlog::install_panic_hook();
        bootlog::stage("single-instance");
        // 1. 单实例（P1-3）：第二实例发激活事件后退出。
        let _mutex = match acquire() {
            Ok(SingleInstance::First(h)) => h,
            Ok(SingleInstance::Second) => {
                // v0.3.7（D-63 B-3）：静默退出正是「双击没反应」的头号嫌疑
                //（旧版本可能仍在托盘里）——退出前弹窗告知已在运行。
                bootlog::note("second instance blocked: already running, notify and exit");
                bootlog::already_running_box();
                return 0;
            }
            Err(e) => {
                bootlog::note(&format!("single instance error: {e}"));
                bootlog::fatal_box("LogiVoltTray 启动失败", &format!("单实例初始化失败：{e}"));
                return 1;
            }
        };

        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        }
        bootlog::stage("dpi-awareness");

        // 2. AUMID + 开始菜单快捷方式（P0-5，尽力而为）。
        bootlog::stage("aumid-shortcut");
        let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("volttray-tray.exe"));
        if let Err(e) = volttray_app::guarded("aumid", || {
            let _ = aumid::set_process_aumid();
        }) {
            bootlog::note(&format!("aumid: {e}"));
        }
        if let Err(e) = volttray_app::guarded("shortcut", || {
            let _ = aumid::ensure_start_menu_shortcut(&exe);
        }) {
            bootlog::note(&format!("shortcut: {e}"));
        }

        // 3. 配置：不存在则按 schema 默认值创建（P1-8）。
        bootlog::stage("config-load");
        let cfg_path = Config::default_path();
        let (cfg, load_warnings) = match Config::load(&cfg_path) {
            Ok((c, w)) => {
                bootlog::note(&format!("config loaded, {} warning(s)", w.len()));
                // v0.3.8（B-3）：警告内容逐条写入日志——上次真机排查只有
                // 数量没有内容，迁移警告（D-56 等）正是本次根因的直接线索。
                for warning in &w {
                    bootlog::note(&format!("config warning: {warning}"));
                }
                (c, w)
            }
            Err(e) => {
                bootlog::note(&format!(
                    "config load failed ({e}), falling back to defaults"
                ));
                let c = Config::default();
                if let Some(parent) = cfg_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(&cfg_path, c.to_toml());
                (c, Vec::new())
            }
        };

        let config_arc = Arc::new(RwLock::new(cfg.clone()));
        let observer = Arc::new(Mutex::new(Observer::new()));
        // P2-1：启动加载的钳制/未知键警告进入诊断流（M9 AC3）。
        // v0.3.3 卫生项①：空警告不再推 ConfigReloaded（噪声事件）。
        if !load_warnings.is_empty() {
            if let Ok(mut o) = observer.lock() {
                o.push(DiagEvent::new(
                    Layer::App,
                    DiagKind::ConfigReloaded {
                        warnings: load_warnings.iter().map(|w| w.to_string()).collect(),
                    },
                ));
            }
        }
        // 共享配置同时喂 AppCore 与设置窗口（GUI ↔ TOML 双向一致，M9/M10）。
        let core = Arc::new(Mutex::new(AppCore::new(config_arc.clone())));

        let (gui_tx, gui_rx) = mpsc::channel::<GuiMsg>();
        let (cmd_tx, cmd_rx) = mpsc::channel::<IoCmd>();
        {
            let core = core.clone();
            let observer = observer.clone();
            let path = cfg_path.clone();
            std::thread::spawn(move || io_loop(core, observer, gui_tx, cmd_rx, path));
        }
        bootlog::stage("io-thread-spawned");

        // 4. 图标缓存（评审 4.1：启动预渲染；v0.3.0：形状/背景/字色/字体 + 低电
        //    提亮档，M9/M11）+ 托盘。
        let dpi = unsafe { GetDpiForSystem() };
        let dark = system_dark_theme();
        let mut cache = cache_from_config(&cfg, dpi_bucket_size(dpi), dark);
        let mut cur_size = dpi_bucket_size(dpi);
        let mut cur_dark = dark;

        let mut state = GuiState::from_config(&cfg);

        bootlog::stage("icon-cache");
        let initial = icon_text(None, false);
        // v0.3.8（D-64 根因修复）：启动首帧取色与 pump 的 percent=None 分支
        // 对齐——分档启用走 Gray 档。v0.3.4 以来的启动代码固定查
        // IconColor::Default，而 v0.3.5 D-56 迁移把「分档关 + 字色 auto」的
        // 旧配置迁成分档开启后，fill 的键集合不含 Default 档，启动即
        // 「default frame missing」退出——即真机日志卡点。
        let initial_color = startup_frame_color(cfg.icon_font_tiers_enabled);
        // v0.3.8（B-1）：图标渲染失败永远不能杀死程序——先记录具体失败
        // 原因（哪个参数 / 哪个 GDI 阶段），再用出厂默认渲染配置重试；
        // 仅出厂默认也失败（系统级 GDI 问题）才走 fatal。
        let first_icon = match cache.icon_for_checked(initial_color, &initial, false) {
            Ok(ic) => ic,
            Err(e) => {
                bootlog::note(&format!(
                    "icon cache init failed with user appearance config: {e}"
                ));
                let factory = Config::default();
                cache = cache_from_config(&factory, dpi_bucket_size(dpi), dark);
                match cache.icon_for_checked(
                    startup_frame_color(factory.icon_font_tiers_enabled),
                    &initial,
                    false,
                ) {
                    Ok(ic) => {
                        bootlog::note(&format!(
                            "appearance config reverted to factory defaults (reason: {e}); \
                             tray continues with default look"
                        ));
                        // B-2：回退原因留给首次打开设置窗口时提示（一次性）。
                        if let Ok(mut slot) = APPEARANCE_FALLBACK_NOTE.lock() {
                            *slot = Some(e.to_string());
                        }
                        ic
                    }
                    Err(e2) => {
                        bootlog::note(&format!(
                            "icon cache init failed with factory defaults too: {e2}"
                        ));
                        bootlog::fatal_box(
                            "LogiVoltTray 启动失败",
                            &format!(
                                "托盘图标预渲染失败（出厂默认渲染配置也无法生成默认帧，\
                                 疑似系统级 GDI 故障）：{e2}。程序即将退出。"
                            ),
                        );
                        return 1;
                    }
                }
            }
        };
        let (menu_obj, mut ids) = build_menu(
            &[],
            None,
            None,
            state.poll_secs,
            state.blink_enabled,
            state.autostart_on,
            state.silent,
        );
        bootlog::stage("tray-build");
        let tray = match TrayIconBuilder::new()
            .with_menu(Box::new(menu_obj))
            .with_tooltip("LogiVoltTray")
            .with_icon(first_icon)
            .with_menu_on_left_click(false)
            .build()
        {
            Ok(t) => t,
            Err(e) => {
                bootlog::note(&format!("tray init failed: {e}"));
                bootlog::fatal_box("LogiVoltTray 启动失败", &format!("创建托盘图标失败：{e}"));
                return 1;
            }
        };

        unsafe {
            let _ = SetTimer(None, 1, 500, None);
        }

        // D-33 被动唤醒：注册鼠标 Raw Input（message-only 窗口，INPUTSINK）。
        // 失败则退化为仅正常轮询节奏——绝不主动保活（不向休眠设备发查询）。
        let raw_input = crate::rawinput::install();
        if raw_input.is_some() {
            bootlog::stage("rawinput-installed");
        } else {
            bootlog::note("rawinput install failed; passive wake degraded to polling");
        }
        let _raw_input = raw_input;

        // v0.3.7（D-63 B-4）：启动成功反馈——托盘应用无主窗口，用户双击后
        // 必须有可感知的确认（静默 toast，不响铃；失败仅落日志，不阻断启动）。
        bootlog::stage("startup-toast");
        if let Err(e) = toast::notify(
            "LogiVoltTray 正在运行",
            "托盘图标位于任务栏右下角通知区域（若未见请展开溢出箭头「^」）；右键图标可打开设置或退出。",
            true,
        ) {
            bootlog::note(&format!("startup toast failed: {e}"));
        }
        let mut last_wake_sent = Instant::now() - Duration::from_secs(10);

        // 5. 消息泵：500ms 定时器驱动事件归并。
        bootlog::stage("message-pump");
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
                        &config_arc,
                        &cfg_path,
                        &exe,
                        &mut ids,
                    );
                } else if msg.message == WM_INPUT {
                    // D-33 被动唤醒：系统层观测到罗技鼠标活动（移动/点击
                    // 本就产生射频流量，属用户行为零额外成本）→ 通知 IO 线程
                    // 装填一次唤醒探测资格。2s 合并防 WM_INPUT 风暴。
                    if rawinput::is_logitech_mouse_input(msg.lParam)
                        && last_wake_sent.elapsed() >= Duration::from_secs(2)
                    {
                        last_wake_sent = Instant::now();
                        let _ = cmd_tx.send(IoCmd::WakeSignal);
                    }
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let _ = KillTimer(None, 1);
        }
        bootlog::stage("message-pump-exit");
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
        cfg_arc: &Arc<RwLock<Config>>,
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

        // 共享配置对账（GUI ↔ TOML 双向一致）：设置窗口直接写共享配置、外部
        // TOML 改动经热重载 swap_config——任何一侧变化都落到 GUI 镜像。
        let mut autostart_target: Option<bool> = None;
        let cfg_changed = match core.lock() {
            Ok(c) => {
                let cfg = c.config();
                let changed = cfg.icon_shape != state.icon_shape
                    || cfg.icon_background != state.icon_bg
                    || cfg.icon_font_color != state.icon_font
                    || cfg.icon_font_family != state.icon_family
                    || cfg.poll_interval_secs != state.poll_secs
                    || cfg.blink_enabled != state.blink_enabled
                    || cfg.low_threshold != state.low_threshold
                    || cfg.notification_silent != state.silent
                    || cfg.autostart != state.autostart_on
                    || cfg.icon_font_tiers_enabled != state.tiers_enabled
                    || cfg.icon_font_tier_thresholds != state.tier_thresholds
                    || cfg.icon_font_tier_colors != state.tier_colors
                    || cfg.icon_text_scale != state.icon_text_scale
                    || cfg.icon_bg_scale != state.icon_bg_scale;
                if changed {
                    state.icon_shape = cfg.icon_shape;
                    state.icon_bg = cfg.icon_background;
                    state.icon_font = cfg.icon_font_color;
                    state.icon_family = cfg.icon_font_family.clone();
                    state.poll_secs = cfg.poll_interval_secs;
                    state.blink_enabled = cfg.blink_enabled;
                    state.low_threshold = cfg.low_threshold;
                    state.silent = cfg.notification_silent;
                    state.tiers_enabled = cfg.icon_font_tiers_enabled;
                    state.tier_thresholds = cfg.icon_font_tier_thresholds.clone();
                    state.tier_colors = cfg.icon_font_tier_colors.clone();
                    state.icon_text_scale = cfg.icon_text_scale;
                    state.icon_bg_scale = cfg.icon_bg_scale;
                    // P2-3：自启键纳入镜像（M13 AC1）——TOML/设置侧改动同步注册表；
                    // 注册表写失败时保留旧镜像值，下个 tick 重试。
                    if cfg.autostart != state.autostart_on {
                        // v0.3.3 卫生项②：注册表 I/O 挪到 core 锁外执行
                        //（此前在锁内做 I/O，且持续失败会每 tick 触发
                        // cfg_changed → 全量 refresh_ui）。
                        autostart_target = Some(cfg.autostart);
                    }
                }
                changed
            }
            Err(_) => false,
        };
        // 自启注册表同步（core 锁外）：成功更新镜像；失败把共享配置回退为
        // 注册表现状（防每 tick 重试刷 UI）并推 Alert 提醒。
        if let Some(target) = autostart_target {
            let ok = if target {
                autostart::enable(exe).is_ok()
            } else {
                autostart::disable().is_ok()
            };
            if ok {
                state.autostart_on = target;
            } else {
                let current = state.autostart_on;
                if let Ok(mut guard) = cfg_arc.write() {
                    guard.autostart = current;
                }
                persist_bool(cfg_path, "autostart", current);
                if let Ok(mut o) = observer.lock() {
                    o.push(DiagEvent::new(
                        Layer::App,
                        DiagKind::Alert {
                            level: "warning".into(),
                            message: format!(
                                "autostart registry sync failed; config reverted to {current}"
                            ),
                        },
                    ));
                }
            }
        }
        if cfg_changed {
            snap_changed = true;
        }

        // DPI / 主题 / 渲染输入任一变化 → 重建图标缓存
        //（rebuild 内部判等，输入未变时零开销）。
        let dpi = unsafe { GetDpiForSystem() };
        let size = dpi_bucket_size(dpi);
        let dark = system_dark_theme();
        if size != *cur_size || dark != *cur_dark || cfg_changed {
            cache.rebuild(
                size,
                dark,
                state.icon_shape,
                state.icon_bg,
                state.icon_font,
                state.icon_family.clone(),
                if state.tiers_enabled {
                    Some(state.tier_colors.clone())
                } else {
                    None
                },
                state.icon_text_scale,
                state.icon_bg_scale,
            );
            *cur_size = size;
            *cur_dark = dark;
            snap_changed = true;
        }

        // 低电闪烁状态机（M11；v0.3.4 #3 抽成 blink 模块，逻辑级测试见 blink.rs）：
        // 500ms tick 翻相位 → 1Hz；活跃 = 开 && 展示设备处于低电触发态 &&
        // 未被点击停闪；解除后重新武装。提亮帧差异由渲染侧 flash_rgb 保证。
        let low_active = shown_device(state).is_some_and(|d| {
            !d.gray
                && core
                    .lock()
                    .map(|c| c.is_low_battery(&d.id))
                    .unwrap_or(false)
        });
        if state.blink.set_low_active(low_active) {
            snap_changed = true;
        }
        let outcome = state.blink.tick(state.blink_enabled);

        if snap_changed || outcome.blinking {
            refresh_ui(tray, state, cache, ids);
        }

        // 托盘图标事件：任意点击本次停闪（M11）；菜单左键弹出不在此列。
        while let Ok(e) = TrayIconEvent::receiver().try_recv() {
            if matches!(e, TrayIconEvent::Click { .. }) && state.blink.acknowledge_click() {
                refresh_ui(tray, state, cache, ids);
            }
        }

        // 菜单事件。
        while let Ok(e) = MenuEvent::receiver().try_recv() {
            if e.id == ids.refresh {
                let _ = cmd_tx.send(IoCmd::RefreshNow);
            } else if e.id == ids.settings {
                let preview = state
                    .snapshot
                    .as_ref()
                    .and_then(|s| pinned_or_default(s, state.pinned.as_deref()))
                    .and_then(|i| s_snapshot_device(state, i))
                    .and_then(|d| d.percent)
                    .map(|p| icon_text(Some(p), false))
                    .unwrap_or_else(|| "50".to_string());
                crate::settings::open_settings(cfg_arc, cfg_path, *cur_dark, &preview);
            } else if e.id == ids.diag {
                export_diag(observer);
            } else if e.id == ids.blink {
                state.blink_enabled = !state.blink_enabled;
                if let Ok(mut guard) = cfg_arc.write() {
                    guard.blink_enabled = state.blink_enabled;
                }
                persist_bool(cfg_path, "blink_enabled", state.blink_enabled);
                refresh_ui(tray, state, cache, ids);
            } else if e.id == ids.autostart {
                let now_on = if state.autostart_on {
                    autostart::disable().is_ok()
                } else {
                    autostart::enable(exe).is_ok()
                };
                state.autostart_on = if state.autostart_on { !now_on } else { now_on };
                if let Ok(mut guard) = cfg_arc.write() {
                    guard.autostart = state.autostart_on;
                }
                persist_bool(cfg_path, "autostart", state.autostart_on);
                refresh_ui(tray, state, cache, ids);
            } else if e.id == ids.silent {
                state.silent = !state.silent;
                if let Ok(mut guard) = cfg_arc.write() {
                    guard.notification_silent = state.silent;
                }
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

    /// 钉选优先，其次默认展示设备；返回快照中的下标。
    fn pinned_or_default(s: &Snapshot, pinned: Option<&str>) -> Option<usize> {
        if let Some(p) = pinned {
            if let Some(i) = s.devices.iter().position(|d| d.id == p) {
                return Some(i);
            }
        }
        s.default_device
    }

    fn s_snapshot_device(state: &GuiState, idx: usize) -> Option<&volttray_app::DeviceSnapshot> {
        state.snapshot.as_ref().and_then(|s| s.devices.get(idx))
    }

    /// 当前展示的设备（钉选优先）。
    fn shown_device(state: &GuiState) -> Option<&volttray_app::DeviceSnapshot> {
        let s = state.snapshot.as_ref()?;
        let idx = pinned_or_default(s, state.pinned.as_deref())?;
        s.devices.get(idx)
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
        // v0.3.3 卫生项（P3-1 缓解）：菜单内容签名未变则跳过 set_menu 重建
        //（闪烁根源 = 每 500ms tick 反复 set_menu 同一内容）。
        let sig = {
            let mut s = format!(
                "{}|p{:?}|b{}|a{}|s{}|d{:?}",
                state.pinned.as_deref().unwrap_or("-"),
                state.poll_secs,
                state.blink_enabled,
                state.autostart_on,
                state.silent,
                default_id.as_deref()
            );
            for d in &devices {
                s.push_str(&format!(
                    "/{}:{}:{:?}:{}:{}",
                    d.id,
                    d.percent
                        .map(|p| p.to_string())
                        .unwrap_or_else(|| "-".into()),
                    d.charging,
                    d.gray,
                    d.state
                ));
            }
            s
        };
        if sig != state.menu_sig {
            let (menu_obj, new_ids) = build_menu(
                &devices,
                default_id.as_deref(),
                state.pinned.as_deref(),
                state.poll_secs,
                state.blink_enabled,
                state.autostart_on,
                state.silent,
            );
            *ids = new_ids;
            tray.set_menu(Some(Box::new(menu_obj)));
            state.menu_sig = sig;
        }

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
        // D-33：休眠 = 灰显但保留最后已知电量数字。gray 只作用于颜色档；
        // 若把文本也打成 "--"，用户会误以为「未读取」——正是本次要修的症状。
        let sleeping = show.is_some_and(|d| d.state == "sleeping");
        let text = icon_text(percent, gray && !sleeping);
        // v0.3.3 #4：分档模式按电量百分比映射档位色（阈值含该值归上一档）；
        // 灰显（未知/休眠离线）仍走灰色档。
        let color = match percent {
            Some(p) if state.tiers_enabled && !gray => {
                IconColor::Tier(font_tier_for_percent(&state.tier_thresholds, p) as u8)
            }
            Some(p) => IconColor::for_percent(p, gray, state.low_threshold),
            None if state.tiers_enabled => IconColor::Gray,
            None => IconColor::Default,
        };
        // M11：低电闪烁 = 常态与提亮档各 500ms 交替；点击托盘后本次停闪。
        let bright = state.blink.bright_frame(state.blink_enabled);
        if let Some(ic) = cache.icon_for(color, &text, bright) {
            let _ = tray.set_icon(Some(ic));
        }
        let tip = match show {
            Some(d) if d.state == "sleeping" => {
                format!(
                    "LogiVoltTray — {} {}（休眠中 · 最后已知电量）",
                    d.name, text
                )
            }
            Some(d) => format!("LogiVoltTray — {} {}", d.name, text),
            None => "LogiVoltTray".to_string(),
        };
        let _ = tray.set_tooltip(Some(&tip));
    }

    /// v0.3.3 #2：把 os error 翻译成可读错误（保留原始错误供诊断）。
    fn friendly_io_error(e: &std::io::Error) -> String {
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
    fn write_diag_export(dir: &Path, filename: &str, json: &str) -> Result<PathBuf, String> {
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
        let filename = format!("LogiVoltTray-diag-export-{secs}.json");
        let msg = match write_diag_export(
            Config::default_path()
                .parent()
                .unwrap_or_else(|| std::path::Path::new(".")),
            &filename,
            &json,
        ) {
            Ok(path) => format!("诊断事件流已脱敏导出：\n{}", path.display()),
            Err(e) => format!("诊断导出失败：{e}"),
        };
        let text = wide(&msg);
        let title = wide("LogiVoltTray 诊断");
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
    mod diag_export_tests {
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
    }
}

fn main() {
    #[cfg(windows)]
    {
        let code = std::panic::catch_unwind(imp::run).unwrap_or(3);
        crate::bootlog::stage(&format!("exit code={code}"));
        std::process::exit(code);
    }
    #[cfg(not(windows))]
    {
        eprintln!("LogiVoltTray 仅支持 Windows；此二进制在非 Windows 平台仅用于编译与单元测试。");
        std::process::exit(2);
    }
}
