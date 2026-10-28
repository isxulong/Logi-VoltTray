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

// 闪烁状态机是纯逻辑（无 Win32 依赖），跨平台编译以便在非 Windows
// 环境跑单元测试；Windows 渲染侧集成测试在模块内单独 cfg(windows) 门控。
#[cfg_attr(not(windows), allow(dead_code))]
mod blink;
#[cfg(windows)]
mod bootlog;
// E8：自 main.rs 拆出的导出叶子模块（诊断导出 + 统计三件套导出）。
#[cfg(windows)]
mod export;
// 帧 LRU / 日志运行期轮转门控是纯逻辑（无 Win32 依赖），跨平台编译以便
// 在非 Windows 环境跑单元测试（v0.3.15，P1-2 / P2-2）。
#[cfg_attr(not(windows), allow(dead_code))]
mod frame_lru;
#[cfg(windows)]
mod icon;
#[cfg_attr(not(windows), allow(dead_code))]
mod logrotate;
// E22（报告口径）：wake 裁决行稳态采样是纯逻辑（无 Win32 依赖），
// 跨平台编译以便在非 Windows 环境跑单元测试。
#[cfg(windows)]
mod menu;
#[cfg_attr(not(windows), allow(dead_code))]
mod wakesampler;
// E8：自 main.rs 拆出的持久化叶子模块（平面 TOML 键改写 + tmp→rename）。
#[cfg(windows)]
mod persist;
#[cfg(windows)]
mod rawinput;
// WP-A（A-1b）：虚拟条目「最后更新于」时间注释的本地时格式化。
#[cfg(windows)]
mod settings;
#[cfg(windows)]
mod single;
#[cfg(windows)]
mod statsexport;
mod timelocal;
#[cfg(windows)]
mod toast;

// WP-B：statsexport 子模块需要 crate 根可见的 IoCmd 与泛化 Save-As。
// E8（红蓝对抗终审）：persist / export 叶子模块自 main.rs 拆出（结构
// 搬动、行为不变）——write_tmp_rename 落 persist 模块，Save-As 落
// export 模块；wall_secs / wide / Telemetry 为共用项，本体在 imp、
// 此处再导出（statsexport 与 imp 互相引用）。
#[cfg(windows)]
pub(crate) use export::pick_save_path_with_filter;
#[cfg(windows)]
pub(crate) use imp::{wall_secs, wide, IoCmd, Telemetry};
#[cfg(windows)]
pub(crate) use persist::write_tmp_rename;

/// v0.3.8（D-64/B-2）：启动时图标渲染回退出厂默认的原因；首次打开设置
/// 窗口时提示一次后取走（None = 未发生回退或已提示过）。
#[cfg(windows)]
static APPEARANCE_FALLBACK_NOTE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// v0.3.22（D-88）：设置窗口「立即重启」请求标志——置位后由主消息循环
/// 统一消费（先 spawn 新进程，再走既有优雅退出；排程代码零改动、
/// 非热生效）。
#[cfg(windows)]
static RESTART_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 请求应用自重启（v0.3.22 D-88）：仅置位，实际重启由主循环执行——
/// 复用既有启动路径（current_exe spawn），不在设置窗口所在调用栈重启。
#[cfg(windows)]
pub fn request_restart() {
    RESTART_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// v0.3.22（质检复核 P0-1）：「立即重启」spawn 子实例时注入的环境
/// 标记——子实例据此进入「轮询等待旧实例释放互斥体」路径，而不是
/// 走普通第二实例的「已在运行」弹窗退出。
#[cfg(windows)]
pub const RESTART_CHILD_ENV: &str = "LOGIVOLTTRAY_RESTART_CHILD";

/// 菜单内容签名：任一分量变化即触发 set_menu 重建（v0.3.3 P3-1 缓解）。
/// R1-F2（T-04③ 修复）：签名并入 `enum_failure_hint`（枚举失败摘要）——
/// 失败候选出现/消失本身即触发重建，兜底提示行不再依赖其余分量变化
/// 才出现。分量与重建分支（build_menu + append_direct_enum_failure_hint）
/// 一一对应，新增分量须同步两处。
/// 第 9 分量 `x{fast_enabled}`（充电快档实验开关）：开关翻转即改变
/// FastPollState 三态判定与「立即刷新」文案，须触发重建。
/// 纯字符串逻辑，置于 crate root（无 cfg 门控）使签名回归测试在
/// Linux 侧可跑（照 frame_lru / wakesampler 跨平台先例）。
#[cfg_attr(not(windows), allow(dead_code))]
#[allow(clippy::too_many_arguments)]
fn menu_content_sig(
    pinned: Option<&str>,
    poll_secs: u64,
    blink_enabled: bool,
    autostart_on: bool,
    silent: bool,
    default_id: Option<&str>,
    devices: &[volttray_app::DeviceSnapshot],
    enum_failure_hint: bool,
    fast_enabled: bool,
) -> String {
    let mut s = format!(
        "{}|p{:?}|b{}|a{}|s{}|d{:?}|h{}|x{}",
        pinned.unwrap_or("-"),
        poll_secs,
        blink_enabled,
        autostart_on,
        silent,
        default_id,
        enum_failure_hint as u8,
        fast_enabled as u8
    );
    for d in devices {
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
}

#[cfg(windows)]
mod imp {
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::sync::{Arc, Mutex, RwLock};
    use std::time::{Duration, Instant};

    use tray_icon::menu::MenuEvent;
    use tray_icon::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
    // WP-A：持久化与通道归因（v3 A-1/A-3）。
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use volttray_app::persist::{self, FlushTuning, LastKnownStore, LoadOutcome};
    use volttray_app::wakechannel::{
        activity_bucket_key, channel_for_raw_path_classified, ActivityKeySource, ChannelMatch,
    };
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::HiDpi::{
        GetDpiForSystem, SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    // T-07：菜单「检查更新」——ShellExecuteW 交系统默认浏览器，零 HTTP 客户端。
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::*;

    use volttray_app::aumid;
    use volttray_app::autostart;
    use volttray_app::config::{
        font_tier_for_percent, IconBackground, IconFontColor, IconShape, RgbColor,
    };
    use volttray_app::diag_export::{ChannelEnumResult, EnumSnapshot};
    use volttray_app::{
        identity_confirmed, pick_promotion, repin, wake_action, wake_arm_set, Alert, AliasSlot,
        AppCore, ChannelLiveness, Config, ConfigWatcher, DeadSlotScan, EnumTransition,
        FastPollState, Observer, PollDecision, Reconnect, SharedObserver, SlotOutcome,
        SlotScanFailure, SlotScanStreak, Snapshot, TimeoutOutcome, UpsertOutcome, WakeAction,
        HANDLE_INVALIDATE_THRESHOLD, SLOT_SCAN_REOPEN_THRESHOLD, WAKE_PROBE_INVALIDATE_THRESHOLD,
    };
    use volttray_device::battery::{
        probe_battery, probe_battery_with_policy, UnknownDiagPolicy, UnknownReason,
    };
    use volttray_device::handshake::{handshake, Handshake, NoResponseCause};
    use volttray_device::identity::read_unit_id;
    use volttray_device::name::read_device_name;
    use volttray_device::state::DeviceState;
    use volttray_device::{classify_pid, Device};
    use volttray_protocol::FeatureTable;

    use crate::wakesampler::SteadyDecisionSampler;
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
    use crate::single::{acquire, acquire_restart_child, RestartAcquire, SingleInstance};
    use crate::toast;
    // E8：persist / export 叶子模块自本文件拆出（结构搬动、行为不变）。
    use crate::export::{export_diag, run_stats_export};
    use crate::persist::{persist_bool, write_tmp_rename};
    #[cfg(windows)]
    use crate::APPEARANCE_FALLBACK_NOTE;

    /// 接收器槽位扫描顺序（参考实现 `_scan_channel` 实证：1..=6 再 0xFF）。
    const SCAN_SLOTS: [u8; 7] = [1, 2, 3, 4, 5, 6, 0xFF];
    /// v0.3.11（F3）：无设备态每轮重扫的汇总日志单会话行数上限
    /// （30s 一轮 ≈ 50 分钟覆盖；防长期无人值守时日志爆量）。
    const RESCAN_LOG_CAP: u32 = 100;
    /// 电压曲线名（v0.2.0：参考实现 13 锚点实证曲线）。
    const CURVE: &str = "li-ion-logi";

    enum GuiMsg {
        Snapshot(Box<Snapshot>),
        Alert,
        /// WP-B：统计导出完成（IO 线程已完成写盘；GUI 弹结果提示）。
        StatsExportDone(String),
    }

    pub(crate) enum IoCmd {
        RefreshNow,
        /// 被动唤醒信号（D-33）：系统层观测到罗技鼠标活动（Raw Input）。
        /// 有注册槽位时仅装填唤醒探测资格；0 注册槽位时（v0.3.11 F5）
        /// 触发一次强制重枚举（设备已被用户物理唤醒，主机侧补读）。
        WakeSignal(Option<String>),
        /// WP-B：耗电统计导出请求（GUI → IO 线程；渲染/写盘在 IO 线程
        /// 后台执行、不阻塞 UI，v3 P2-③ 口径；只读本机文件，零设备查询）。
        ExportStats(crate::statsexport::StatsExportRequest),
        Shutdown,
    }

    /// v0.3.15（P2-1②配套）：IO → GUI 的「队列有货」线程消息。
    /// 闪烁定时器改事件驱动后消息泵不再有 500ms 心跳，IO 线程产出
    /// 快照/告警时必须显式唤醒 GetMessageW，否则空闲期 UI 更新被
    /// 无限推迟。WM_APP 段自定义，不与系统/控件消息冲突。
    const WM_APP_GUI: u32 = WM_APP + 0x56;

    /// GUI 通道出口：发消息 + 唤醒消息泵（PostThreadMessageW 失败仅
    /// 意味着泵尚未启动/正在退出，消息仍留在 channel 里，不丢数据）。
    struct GuiSink {
        tx: mpsc::Sender<GuiMsg>,
        tid: u32,
    }

    impl GuiSink {
        fn send(&self, msg: GuiMsg) {
            let _ = self.tx.send(msg);
            // SAFETY(E9): tid 为 GUI 线程启动时记录的本进程线程 id；PostThreadMessageW 仅投递值类型消息号，失败忽略不产生悬垂。
            unsafe {
                let _ = PostThreadMessageW(self.tid, WM_APP_GUI, WPARAM(0), LPARAM(0));
            }
        }
    }

    /// R-11（v0.3.17 真机终审）：帧级遥测（诊断导出 summary 追加字段）。
    /// 写 / 读计数、feature_index 分类、平均写间隔——在 SharedChannel 的
    /// write / read 上挂钩（get_feature_report 为控制传输、注册期一次性
    /// 事务，不计入）。E8（红蓝对抗终审）：原进程级静态量已收编进
    /// Telemetry（由 run 持有、Arc 穿线，见下方）；const fn new 依赖
    /// Mutex / BTreeMap / AtomicU64 的 const 构造（与
    /// APPEARANCE_FALLBACK_NOTE 同款，工具链 1.90 均支持）。
    struct FrameStatsInner {
        last_tx: Option<Instant>,
        tx_interval_sum_ms: u64,
        tx_interval_count: u64,
        feature_tx: std::collections::BTreeMap<String, u64>,
        feature_rx: std::collections::BTreeMap<String, u64>,
    }

    pub(crate) struct FrameStats {
        pub(crate) tx: AtomicU64,
        pub(crate) rx: AtomicU64,
        inner: Mutex<FrameStatsInner>,
    }

    impl FrameStats {
        const fn new() -> Self {
            Self {
                tx: AtomicU64::new(0),
                rx: AtomicU64::new(0),
                inner: Mutex::new(FrameStatsInner {
                    last_tx: None,
                    tx_interval_sum_ms: 0,
                    tx_interval_count: 0,
                    feature_tx: std::collections::BTreeMap::new(),
                    feature_rx: std::collections::BTreeMap::new(),
                }),
            }
        }

        fn note_tx(&self, frame: &[u8], now: Instant) {
            self.tx.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut g) = self.inner.lock() {
                if let Some(last) = g.last_tx {
                    g.tx_interval_sum_ms = g
                        .tx_interval_sum_ms
                        .saturating_add(now.duration_since(last).as_millis() as u64);
                    g.tx_interval_count = g.tx_interval_count.saturating_add(1);
                }
                g.last_tx = Some(now);
                // BF-4：HID++ 长 / 短帧第 3 字节（index 2）在 2.0 帧下为
                // feature_index（功能索引，非 feature_id 功能号），1.0 帧
                // 下为 sub_id——键名以帧格式实际语义为准。
                if let Some(f) = frame.get(2) {
                    *g.feature_tx.entry(format!("0x{f:02X}")).or_insert(0) += 1;
                }
            }
        }

        fn note_rx(&self, frame: &[u8]) {
            self.rx.fetch_add(1, Ordering::Relaxed);
            if let Ok(mut g) = self.inner.lock() {
                if let Some(f) = frame.get(2) {
                    *g.feature_rx.entry(format!("0x{f:02X}")).or_insert(0) += 1;
                }
            }
        }

        pub(crate) fn avg_tx_interval_ms(&self) -> Option<u64> {
            let g = self.inner.lock().ok()?;
            if g.tx_interval_count == 0 {
                None
            } else {
                Some(g.tx_interval_sum_ms / g.tx_interval_count)
            }
        }

        pub(crate) fn feature_tx(&self) -> std::collections::BTreeMap<String, u64> {
            self.inner
                .lock()
                .map(|g| g.feature_tx.clone())
                .unwrap_or_default()
        }

        pub(crate) fn feature_rx(&self) -> std::collections::BTreeMap<String, u64> {
            self.inner
                .lock()
                .map(|g| g.feature_rx.clone())
                .unwrap_or_default()
        }
    }

    /// 同一接收器通道的句柄共享：多个槽位（设备）共用一对 HID collection。
    /// E8：遥测计数器随通道句柄共享（原全局 FRAME_STATS 静态量，
    /// 行为不变——同一 Telemetry 实例的 frames）。
    #[derive(Clone)]
    struct SharedChannel(Arc<Mutex<HidDeviceChannel>>, Arc<FrameStats>);

    impl HidChannel for SharedChannel {
        fn write(&mut self, d: &[u8]) -> Result<usize, IoError> {
            // R-11：写成功记帧遥测（feature_index 分类 + 平均写间隔样本）。
            let r = self
                .0
                .lock()
                .map_err(|_| IoError::Other("channel mutex poisoned".into()))?
                .write(d);
            if r.is_ok() {
                self.1.note_tx(d, Instant::now());
            }
            r
        }
        fn read(&mut self, buf: &mut [u8], t: Duration) -> Result<usize, IoError> {
            // R-11：读到非空帧记读遥测（超时 Ok(0) 不计）。
            let r = self
                .0
                .lock()
                .map_err(|_| IoError::Other("channel mutex poisoned".into()))?
                .read(buf, t);
            if let Ok(n) = &r {
                if *n > 0 {
                    self.1.note_rx(&buf[..*n]);
                }
            }
            r
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
        /// T6：注册期一次性读到的物理设备 unitId（None = 身份不明，
        /// 走「不合并」降级路径）。只读一次，不进任何周期轮询路径。
        unit_id: Option<String>,
        /// T6：同一物理设备的重复端点（别名槽位）——不参与轮询、不进
        /// 菜单、不触发 toast；主端点所在通道消失时可被晋级为新主端点。
        alias: bool,
    }

    struct ChannelState {
        key: String,
        info: ChannelInfo,
        shared: SharedChannel,
        /// 通道句柄活性裁决（v0.3.10，D-67）：按「轮」累计静默连击——
        /// 整轮零应答才记 1 次；多设备同睡（正常休眠边界内）不判死；
        /// 唤醒探测静默单独计数。判死才作废句柄 + 强制重枚举自愈。
        liveness: ChannelLiveness,
        /// 通道级「扫槽连败」裁决（v0.3.11，F2/D-68）：0 槽位时轮询
        /// outcomes 恒空、liveness 永不裁决——扫槽路径上的句柄死亡观测点；
        /// 连续全败且含 I/O 类才强制重开，纯超时（深睡）不触发。
        scan_streak: SlotScanStreak,
        /// R-1（v0.3.17）：0xFF 接收器槽电量探测 error_response 的 run 内
        /// 负缓存——本通道句柄存活期内失败过一次即不再重复探测（结构性
        /// 噪声，非设备异常）；force 枚举句柄重开时清零。
        ff_battery_probe_failed: bool,
        /// R-2（v0.3.17）：死槽扫描冷却——连续 K 轮握手无设备应答
        ///（Timeout 或 NoDevice，E10 细分口径）的槽位降频为每 K 轮
        /// 扫 1 次；force 枚举清零（手动刷新/动鼠标立即穿透）。
        dead_slots: DeadSlotScan,
        slots: Vec<SlotState>,
    }

    /// WP-A：墙钟 unix 秒（持久化 last_seen 用；取不到返回 0，不 panic）。
    pub(crate) fn wall_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    pub(crate) fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
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
    /// 单次扫槽的边效应记录（R-1/R-2，v0.3.17）：见 try_slot `edge` 参数。
    #[derive(Default)]
    struct SlotScanEdge {
        /// 本轮该槽位 0xFF 电量探测返回 error_response（结构性失败）。
        ff_battery_probe_failed: bool,
        /// 本轮该槽位握手阶段「无设备应答」——纯超时不应答（Timeout）或
        /// 接收器应答该槽位无设备（NoDevice，E10 细分）；喂死槽冷却裁决。
        handshake_no_response: bool,
        /// E22：本次注册把一台 Sleeping/Disconnected 设备捞回在线
        ///（upsert 前设备处于休眠/断开态）；供调用方在唤醒信号窗口内
        /// 补记 wake_recovery_via_reenum。
        recovered_via_scan: bool,
    }

    /// 0xFF 负缓存门（E10 拆为纯函数以便回归测试逐行对照）：
    /// cached=本 run 内该通道 0xFF 探测已出现 error_response；
    /// cache_applies=握手结论允许负缓存（`Handshake::ff_negative_cache_applies`）。
    /// 返回 true = 缓存抑制本次探测；false = 照常探测（未命中，或可信
    /// HID++1 直连设备——由调用方清缓存放行，AF-6）。
    fn ff_probe_suppressed(slot: u8, cached: bool, cache_applies: bool) -> bool {
        slot == 0xFF && cached && cache_applies
    }

    /// 返回注册成功的槽位；Err 携带失败类别（v0.3.11 F1：供通道级
    /// 「扫槽连败」裁决区分「设备深睡不应答」与「句柄可疑」，F2）。
    ///
    /// T6：注册期在既有握手/探测事务之后一次性读取 unitId（设备刚应答过，
    /// 必然醒着；仅这一次，不进周期轮询——电池红线）。upsert 命中同一
    /// 物理身份时发生 rebind：返回值第二元携带 (old_id, new_id)，调用方
    /// 负责把 old_id 对应的槽位标记为别名（借用约束，见枚举循环）。
    ///
    /// E22：唤醒恢复窗口——唤醒信号后该时长内经整轮重枚举把休眠/断开
    /// 设备捞回注册，计为 wake_recovery_via_reenum（与唤醒探测直接命中
    /// 的 wake_successes 区分，归因「信号来了但探测没抓住」的路径）。
    const WAKE_RECOVERY_WINDOW: Duration = Duration::from_secs(30);

    /// E22：本次扫槽捞回了休眠/断开设备且处于唤醒信号窗口内时，补记
    /// 一次重枚举唤醒恢复（遥测 + bootlog，零新增设备查询）。
    fn note_wake_recovery_via_reenum(
        telemetry: &Telemetry,
        recovered_via_scan: bool,
        last_wake_signal_at: Option<Instant>,
        id: &str,
        now: Instant,
    ) {
        if recovered_via_scan
            && last_wake_signal_at.is_some_and(|t| now.duration_since(t) <= WAKE_RECOVERY_WINDOW)
        {
            telemetry
                .wake_recovery_via_reenum
                .fetch_add(1, Ordering::Relaxed);
            bootlog::note(&format!(
                "wake recovery via re-enumeration: id={id} (registered within {}s of wake signal)",
                WAKE_RECOVERY_WINDOW.as_secs()
            ));
        }
    }

    /// R-1/R-2（v0.3.17）：`edge` 带出 try_slot 内部才能观测的两类事实——
    /// 0xFF 电量探测 error_response（写通道级负缓存）与握手无设备应答
    ///（Timeout 或 NoDevice，喂死槽冷却裁决）；借用约束下经 &mut 带出，
    /// 不改返回类型。
    fn try_slot(
        core: &Arc<Mutex<AppCore>>,
        observer: &Arc<Mutex<Observer>>,
        shared: &SharedChannel,
        info: &ChannelInfo,
        slot: u8,
        edge: &mut SlotScanEdge,
        stats: &mut volttray_app::stats::StatsStore,
    ) -> Result<(SlotState, Option<(String, String)>), SlotScanFailure> {
        let id = format!("{}#{}", info.key(), slot);
        let mut t = Transport::new(shared.clone(), SharedObserver(observer.clone()));
        let hs = handshake(&mut t, slot);
        // E10：0xFF 负缓存适用性按握手结论判定（须在 match 消费 hs 前取）。
        // 修复前 0x01 幽灵槽被误判 Hidpp1，走下方 is_hidpp1 清缓存分支导致
        // 负缓存永不生效；现由 Hidpp1Ghost 携带「未证实 1.0 候选」语义，
        // 缓存对其适用。可信 HID++1 直连设备仍不适用（AF-6）。
        let ff_cache_applies = hs.ff_negative_cache_applies();
        let (features, is_hidpp1) = match hs {
            Handshake::Hidpp2(table) => (table, false),
            // 可信 1.0 设备与 0x01 幽灵候选同走 1.0 寄存器探测链——真实
            // 设备的注册通道保留；两者差异仅在负缓存适用性（上）。
            Handshake::Hidpp1 | Handshake::Hidpp1Ghost => (FeatureTable::new(), true),
            Handshake::NoResponse(cause) => {
                // E10：死槽冷却喂入口径由 NoResponseCause 自带——Timeout
                // （直连深睡/空槽）与 NoDevice（接收器 0x08「槽位无设备」）
                // 计入连击；Io/Protocol 在冷却语义里算「槽位活着」，I/O
                // 证据保持热度留给 SlotScanStreak 裁决句柄死亡。
                // 修复前仅 Timeout 置位，接收器空槽（0x08）永不喂入冷却。
                edge.handshake_no_response = cause.feeds_dead_slot_cooldown();
                // v0.3.11（F1）：slot scan 日志区分超时与 I/O 类失败——
                // 死句柄与深睡设备在日志中可分辨（此前同判 no response）。
                match cause {
                    NoResponseCause::Io => bootlog::note(&format!(
                        "slot scan: {id} io error (handle suspect) (slot skipped)"
                    )),
                    NoResponseCause::NoDevice => {
                        bootlog::note(&format!("slot scan: {id} no device at slot (slot skipped)"))
                    }
                    _ => bootlog::note(&format!(
                        "slot scan: {id} handshake no response (slot skipped)"
                    )),
                }
                return Err(match cause {
                    NoResponseCause::Timeout => SlotScanFailure::Timeout,
                    NoResponseCause::Io => SlotScanFailure::Io,
                    // NoDevice：接收器回了字节 = 句柄活着，与 Protocol 同归
                    // Other——不喂 SlotScanStreak 的句柄死亡裁决（防误杀）。
                    NoResponseCause::Protocol | NoResponseCause::NoDevice => SlotScanFailure::Other,
                });
            }
        };
        // R-1（v0.3.17）：0xFF 接收器槽电量探测的 run 内负缓存——本通道
        // 句柄存活期内 error_response 已出现过一次即直接跳过探测（每轮
        // 枚举必现的结构性噪声，省一次注定失败的查询事务与日志）。握手
        // 保留（槽位发现仍依赖它，仅 1 次短事务）；仅缓存 ErrorResponse
        // 一类结构性失败，超时不缓存——HID++1 直连设备可能深睡后应答，
        // 缓存超时会把它永久挡在注册门外。force 枚举句柄重开时清零。
        // E10：缓存命中时的分支条件由 is_hidpp1 改为 ff_cache_applies——
        // 0x01 幽灵槽（Hidpp1Ghost）不再每轮清缓存，负缓存真实生效。
        if ff_probe_suppressed(slot, edge.ff_battery_probe_failed, ff_cache_applies) {
            bootlog::note(&format!(
                "slot scan: {id} battery probe skipped (0xFF failure cached this run)"
            ));
            return Err(SlotScanFailure::Other);
        }
        if slot == 0xFF && edge.ff_battery_probe_failed {
            // 可信 HID++1 直连设备握手成功：缓存的结构性失败对其不适用
            //（终审 R-1：握手保留 1 帧用于发现 HID++1 直连设备）——
            // 清缓存照常探测注册，否则该设备本 run 内永不注册（AF-6）。
            edge.ff_battery_probe_failed = false;
        }
        // 电量探测：失败不注册（BatteryUnknown 事件已由 probe_battery 记录原因分类）。
        // F5 次修 P2：#255（0xFF 接收器直连槽）的 error_response 是握手
        // 误分类后的结构性噪声（每轮枚举必现，非设备异常），按策略压掉
        // BatteryUnknown 诊断事件，仅在此留 debug 级 bootlog；真实设备
        // 槽位照常派发（Emit）。
        let unknown_policy = if slot == 0xFF {
            UnknownDiagPolicy::SuppressReceiverSlotErrorResponse
        } else {
            UnknownDiagPolicy::Emit
        };
        let reading = match probe_battery_with_policy(
            &mut t,
            &id,
            slot,
            &features,
            CURVE,
            is_hidpp1,
            unknown_policy,
        ) {
            Ok(r) => r,
            Err(reason) => {
                bootlog::note(&format!(
                    "slot scan: {id} battery query failed reason={} (slot skipped){}",
                    reason.code(),
                    if slot == 0xFF && reason == UnknownReason::ErrorResponse {
                        " [receiver-slot noise: BatteryUnknown event suppressed]"
                    } else {
                        ""
                    }
                ));
                // R-1（v0.3.17）：仅 error_response（结构性失败）进负缓存；
                // 超时/设备离开不缓存（可能是真设备暂不应答）。
                if slot == 0xFF && reason == UnknownReason::ErrorResponse {
                    edge.ff_battery_probe_failed = true;
                }
                // v0.3.11（F1/F2）：探测阶段 I/O 类失败同样是句柄可疑证据。
                return Err(match reason {
                    UnknownReason::QueryTimeout => SlotScanFailure::Timeout,
                    UnknownReason::IoFailure | UnknownReason::DeviceGone => SlotScanFailure::Io,
                    _ => SlotScanFailure::Other,
                });
            }
        };
        // v0.3.0「?」修复：优先取真实设备名（HID++ DEVICE_NAME / 接收器配对名
        // 寄存器 0x2B5，见 name.rs 来源链）；取不到用干净回退，不出裸「?」。
        let raw_name = read_device_name(&mut t, slot, &features, is_hidpp1)
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| slot_name(info, slot));
        // T6：一次性读取物理身份（HID++1 设备 feature 表为空，read_unit_id
        // 内部直接返回 None、零通信）。取不到走「不合并」降级（验收 3）。
        let unit_id = read_unit_id(&mut t, slot, &features);
        if unit_id.is_none() && !is_hidpp1 {
            // 降级路径可观测标记（诊断导出可见）：身份不明 → 不合并。
            bootlog::note(&format!(
                "slot scan: {id} unitId unavailable; physical identity unproven (no merge)"
            ));
            if let Ok(mut o) = observer.lock() {
                o.push(DiagEvent::new(
                    Layer::App,
                    DiagKind::Info {
                        message: format!(
                            "physical identity unproven (unitId unavailable): {id}; \
                             fallback = no-merge"
                        ),
                    },
                ));
            }
        }
        let mut dev = Device::new(
            id.clone(),
            &raw_name,
            classify_pid(info.pid),
            slot,
            is_hidpp1,
        );
        if let Some(u) = &unit_id {
            dev.set_unit_id(u);
        }
        let name = dev.name.clone();
        let now = Instant::now();
        let mut rebound: Option<(String, String)> = None;
        if let Ok(mut c) = core.lock() {
            // E22（小修批第 2 项修复）：注册前离线判定必须在 upsert 之前
            // 捕获——upsert 的 MergedVirtual 分支会把条目状态直接迁到
            // Online，事后再读在册状态恒为在线、wake_recovery_via_reenum
            // 恒 0（v0.3.23 真机反证：两次 no-device 恢复均漏计）。
            let pre_reg_offline = c.pre_registration_offline(&dev);
            match c.upsert_device(dev) {
                UpsertOutcome::Rebound { old_id, new_id } => {
                    bootlog::note(&format!(
                        "slot scan: physical identity match; rebind {old_id} -> {new_id} (old endpoint becomes alias)"
                    ));
                    if let Ok(mut o) = observer.lock() {
                        o.push(DiagEvent::new(
                            Layer::App,
                            DiagKind::DeviceRebound {
                                old_id: old_id.clone(),
                                new_id: new_id.clone(),
                            },
                        ));
                    }
                    rebound = Some((old_id, new_id));
                }
                UpsertOutcome::MergedVirtual { old_id, new_id } => {
                    // WP-A（A-1b）：真实设备注册合并同名虚拟条目——最后已知
                    // 读数已在 core 内并入，虚拟行从菜单消失（AC-A1b）。
                    bootlog::note(&format!(
                        "slot scan: merged virtual entry {old_id} into real {new_id} \
                         (physical key match; last-known carried over)"
                    ));
                }
                UpsertOutcome::Inserted | UpsertOutcome::Updated => {}
            }
            // rebind 后在册条目 id 已是本槽位 id，按本 id 记录读数即可。
            // WP-B：扫描路径成功读数同口径落统计事件（变化驱动过滤在
            // store 内）。设备刚应答 = 已醒；重注册场景（休眠中槽位重扫
            // 注册）pre-read 为 Sleeping 则记 wake-first-read。
            let was_sleeping = c
                .entries()
                .iter()
                .any(|e| e.device.id == id && e.device.state == DeviceState::Sleeping);
            // E22：注册前处于休眠/断开态 = 本次扫槽把设备捞回在线，
            // 供调用方在唤醒信号窗口内补记 wake_recovery_via_reenum。
            // 小修批第 2 项：改用 upsert 前捕获的 pre_reg_offline（理由
            // 见上）。
            edge.recovered_via_scan = pre_reg_offline;
            let (pct, chg) = (reading.percent, reading.charging);
            c.record_success(&id, reading, now);
            let pk = c
                .entries()
                .iter()
                .find(|e| e.device.id == id)
                .map(|e| e.device.physical_key.as_str().to_string())
                .unwrap_or_default();
            if !pk.is_empty() {
                stats.note_reading(
                    &pk,
                    &name,
                    pct,
                    chg,
                    volttray_app::stats::read_source(was_sleeping, chg),
                    wall_secs() as i64,
                    now,
                );
            }
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
        Ok((
            SlotState {
                id,
                index: slot,
                features,
                is_hidpp1,
                hard_failures: 0,
                unit_id,
                alias: false,
            },
            rebound,
        ))
    }

    /// T6：主端点槽位被移除时的晋级善后——从仍存活（`alive` 判定）的通道
    /// 里挑一个同 unitId 的别名槽位，取消其别名标记并把台账条目重绑到
    /// 该端点（身份连续：电量 / 低电追踪 / 超时连击 / pin 选定不丢）。
    /// 挑选逻辑在 app 层（`pick_promotion`，纯函数单测覆盖）；本函数只做
    /// 投影与落账。晋级成功返回 true；无候选（或身份不明）返回 false，
    /// 调用方按各自原有语义继续（移除 / 置离线）。
    ///
    /// 红蓝对抗 P1-1（必修，D-71）：槽位 unit_id 是注册期一次性冻结值——
    /// 接收器槽位在别名存续期间被 RF 侧重配对（Logi 配对软件，纯 RF 操作、
    /// USB 枚举签名不变、不触发重扫）后，冻结值不再代表端点当前身份，
    /// 凭冻结值晋级会把台账误绑到另一台物理鼠标的端点并永久压制它。
    /// 因此晋级前对候选别名端点**一次性重读 unitId 验明**（`identity_
    /// confirmed`，纯函数单测覆盖）：一致才晋级；读不到或不匹配则放弃
    /// 晋级并丢弃该槽位——其别名身份已被证伪 / 无法证实，保留它会让
    /// 端点上真实设备被永久压制（槽位存在即跳过重扫），丢弃后下一轮
    /// 枚举按常规注册流程重新发现。电池红线：重读仅由「主端点消失 /
    /// 晋级事件」触发、每事件至多一次，与注册期 read_unit_id 同级，
    /// 不进入任何周期轮询路径。
    fn try_promote(
        core: &mut AppCore,
        observer: &Arc<Mutex<Observer>>,
        channels: &mut [ChannelState],
        removed: &SlotState,
        alive: &dyn Fn(&str) -> bool,
    ) -> bool {
        let Some(unit) = removed.unit_id.as_deref() else {
            return false;
        };
        let mut views: Vec<AliasSlot> = Vec::new();
        let mut locs: Vec<(usize, usize)> = Vec::new();
        for (ci, c) in channels.iter().enumerate() {
            if !alive(&c.key) {
                continue;
            }
            for (si, x) in c.slots.iter().enumerate() {
                views.push(AliasSlot {
                    channel_alive: true,
                    id: x.id.clone(),
                    alias: x.alias,
                    unit_id: x.unit_id.clone(),
                });
                locs.push((ci, si));
            }
        }
        let Some(p) = pick_promotion(unit, &views) else {
            return false;
        };
        let (ci, si) = locs[p];
        // P1-1：晋级前验明候选端点当前身份（事件触发，每事件至多一次）。
        let (cand_id, cand_index, cand_features) = {
            let t = &channels[ci].slots[si];
            (t.id.clone(), t.index, t.features.clone())
        };
        let reread = {
            let mut t = Transport::new(
                channels[ci].shared.clone(),
                SharedObserver(observer.clone()),
            );
            read_unit_id(&mut t, cand_index, &cand_features)
        };
        if !identity_confirmed(unit, reread.as_deref()) {
            // 验明失败：放弃晋级 + 丢弃槽位（恢复该端点的常规重注册，
            // 避免真实设备被陈旧别名永久压制），调用方按现状移除条目。
            channels[ci].slots.retain(|x| x.id != cand_id);
            bootlog::note(&format!(
                "promotion aborted: alias {cand_id} identity re-read {reread:?} != frozen {unit}; slot dropped for re-registration"
            ));
            if let Ok(mut o) = observer.lock() {
                o.push(DiagEvent::new(
                    Layer::App,
                    DiagKind::Info {
                        message: format!(
                            "promotion aborted: alias {cand_id} failed identity re-verification \
                             (frozen unitId no longer matches endpoint); slot dropped, \
                             endpoint will re-register on next enum"
                        ),
                    },
                ));
            }
            return false;
        }
        let (new_id, new_index, pid) = {
            let t = &mut channels[ci].slots[si];
            t.alias = false;
            (t.id.clone(), t.index, channels[ci].info.pid)
        };
        // P2-4（记录放行，D-71）：rebind 返回 false（条目不存在）时别名
        // 已清、true 已返——当前四条调用路径均保证条目存在（晋级必先于
        // remove/record_failure，且槽位必注册过），现实不可触发，按裁决
        // 记录不改代码。
        core.rebind_device_id(&removed.id, &new_id, new_index, classify_pid(pid));
        bootlog::note(&format!(
            "primary endpoint removed: {} rebind -> promoted alias {} (identity re-verified, preserved)",
            removed.id, new_id
        ));
        if let Ok(mut o) = observer.lock() {
            o.push(DiagEvent::new(
                Layer::App,
                DiagKind::DeviceRebound {
                    old_id: removed.id.clone(),
                    new_id,
                },
            ));
        }
        true
    }

    /// R-11（v0.3.17）：唤醒路径遥测（诊断导出 summary 追加字段）。
    /// E8（红蓝对抗终审）：原 WAKE_* 进程级静态量与 FRAME_STATS 一并
    /// 收编进具名 struct（Telemetry）——由 run() 创建、经 Arc 穿线到
    /// io_loop / pump / export_diag，取代全局静态可变状态；计数语义
    /// 与读写时点逐字节不变。
    /// arm_all 装填数 / 唤醒成功数 / attribution miss 子分类（no-path /
    /// zero-match / no-fingerprint / ambiguous；E11 把原 no-match 桶
    /// 拆为零候选匹配与指纹不可解析两类，与「多候选同指纹」的歧义
    /// 相区分）。
    pub(crate) struct Telemetry {
        pub(crate) frames: Arc<FrameStats>,
        pub(crate) wake_arm_all_fills: AtomicU64,
        pub(crate) wake_successes: AtomicU64,
        /// E22：唤醒信号窗口内经整轮重枚举恢复注册的槽位数——与
        /// wake_successes（唤醒探测直接命中）区分，归因「信号来了但
        /// 归因/探测没抓住、靠重枚举捞回」的恢复路径。
        pub(crate) wake_recovery_via_reenum: AtomicU64,
        pub(crate) wake_miss_no_path: AtomicU64,
        pub(crate) wake_miss_zero_match: AtomicU64,
        pub(crate) wake_miss_no_fingerprint: AtomicU64,
        pub(crate) wake_miss_ambiguous: AtomicU64,
    }

    impl Telemetry {
        pub(crate) fn new() -> Self {
            Self {
                frames: Arc::new(FrameStats::new()),
                wake_arm_all_fills: AtomicU64::new(0),
                wake_successes: AtomicU64::new(0),
                wake_recovery_via_reenum: AtomicU64::new(0),
                wake_miss_no_path: AtomicU64::new(0),
                wake_miss_zero_match: AtomicU64::new(0),
                wake_miss_no_fingerprint: AtomicU64::new(0),
                wake_miss_ambiguous: AtomicU64::new(0),
            }
        }
    }

    /// R-4（v0.3.17）：attribution miss 日志采样——BT/有线系统性失配
    /// 场景 miss 随每次鼠标活动高频出现，逐条落 bootlog 会刷屏：每个
    /// 60s 窗口只落第 1 条，窗口内被压制的条数合并进下一条汇报
    /// （「每分钟 1 条 + 计数」）。
    struct MissLogSampler {
        window_start: Option<Instant>,
        suppressed: u64,
    }

    impl MissLogSampler {
        fn new() -> Self {
            Self {
                window_start: None,
                suppressed: 0,
            }
        }

        /// 记录一次 miss，返回本次应落的日志行（None = 窗口内压制）。
        fn note_miss(&mut self, msg: &str, now: Instant) -> Option<String> {
            match self.window_start {
                Some(t) if now.duration_since(t) < Duration::from_secs(60) => {
                    self.suppressed = self.suppressed.saturating_add(1);
                    None
                }
                _ => {
                    let suppressed = self.suppressed;
                    self.window_start = Some(now);
                    self.suppressed = 0;
                    if suppressed > 0 {
                        Some(format!(
                            "{msg} (suppressed {suppressed} similar in last 60s)"
                        ))
                    } else {
                        Some(msg.to_string())
                    }
                }
            }
        }

        /// 会话收尾冲出 pending 压制计数（小修批第 3 项，口径对齐）：
        /// 压制数平时随下个窗口首条汇报，进程退出时尾部最后一个窗口的
        /// 压制数再无机会落盘——diag 计数器（per-run 全量）与日志行
        /// 因此永不可对平。Shutdown 路径调用本方法把尾部冲出，使
        /// 对账公式成立：计数器值 = 可见 miss 行数 + Σ suppressed
        /// 汇报（含本尾部行）。注意本采样器跨 4 个 miss 子类合并计数，
        /// 子类级精确对账以 diag 计数器为准、日志为佐证。
        fn flush_tail(&mut self) -> Option<String> {
            if self.suppressed == 0 {
                return None;
            }
            let n = self.suppressed;
            self.suppressed = 0;
            Some(format!(
                "wake attribution miss sampler tail: suppressed {n} similar flushed at session end"
            ))
        }
    }

    /// IO 线程：枚举 → 握手 → 周期轮询。真机行为未验证（见 docs/decisions.md）。
    /// v0.3.12（D-69）：`enum_snap` 接收每轮枚举快照（导出诊断时 GUI 线程读取）。
    #[allow(clippy::too_many_arguments)]
    fn io_loop(
        core: Arc<Mutex<AppCore>>,
        observer: Arc<Mutex<Observer>>,
        gui: GuiSink,
        cmd: mpsc::Receiver<IoCmd>,
        cfg_path: PathBuf,
        enum_snap: Arc<Mutex<Option<EnumSnapshot>>>,
        channel_map_miss: Arc<AtomicU64>,
        // E8：遥测状态（原 FRAME_STATS / WAKE_* 进程级静态量）由 run
        // 创建并穿线——具名 struct 由 main 持有，取代全局静态。
        telemetry: Arc<Telemetry>,
        // WP-B（I-02）：Shutdown 落盘完成的回执——GUI 菜单退出 /
        // WM_ENDSESSION 承接窗口据此等待 run_stop + 双 store flush 完成。
        shutdown_ack: mpsc::Sender<()>,
    ) {
        let mut channels: Vec<ChannelState> = Vec::new();
        // E22：最近一次唤醒信号时刻——窗口内经整轮重枚举捞回休眠/断开
        // 设备时补记 wake_recovery_via_reenum（遥测，见 try_slot 调用点）。
        let mut last_wake_signal_at: Option<Instant> = None;
        let mut watcher = ConfigWatcher::new(cfg_path);
        // v0.3.6（D-62）：热重载告警去重——同一组迁移告警只上报一次。
        let mut last_reload_warnings: Vec<String> = Vec::new();
        // v0.3.9（D-65）：重枚举/重连状态机——无设备态退避重试、立即刷新
        // 强制重枚举、句柄失效自愈的唯一裁决点（纯逻辑，app 层单测覆盖）。
        let mut reconnect = Reconnect::new();
        // WP-A（v3 A-1/A-1b）：last_known.json 持久化——启动加载（损坏
        // 回退 .bak 由 store 内部处理），随后把最后已知读数播种为虚拟条目
        // （重启即可灰显最后已知电量，零查询、next_due 恒 None）。
        // 相位二（第 4 项）：experimental_persist_last_known=false 时整体
        // 停用——不加载存量（不触发 .bak 改名副作用）、不播种、不落盘。
        // 开关在启动时读取一次，运行期改配置需重启生效（设置页已标注）。
        let persist_enabled = core
            .lock()
            .map(|c| c.config().experimental_persist_last_known)
            .unwrap_or(true);
        let (mut store, load_outcome) = if persist_enabled {
            LastKnownStore::load(persist::default_path(), FlushTuning::default())
        } else {
            bootlog::note(
                "experimental: persist_last_known OFF — last_known.json disabled (no load, no seeding, no writes)",
            );
            (
                LastKnownStore::disabled(persist::default_path()),
                LoadOutcome::Empty,
            )
        };
        match load_outcome {
            LoadOutcome::Empty => bootlog::note("last_known.json: no usable store (fresh start)"),
            LoadOutcome::Loaded(n) => {
                bootlog::note(&format!("last_known.json: loaded {n} record(s)"))
            }
            LoadOutcome::Recovered => bootlog::note(
                "last_known.json: primary corrupted/unreadable; renamed to .bak, empty fallback",
            ),
        }
        if let Ok(mut c) = core.lock() {
            let mut seeded = 0usize;
            for rec in store.records() {
                if c.seed_virtual(&rec) {
                    seeded += 1;
                }
            }
            if seeded > 0 {
                bootlog::note(&format!(
                    "last_known.json: seeded {seeded} virtual entries (last-known display)"
                ));
            }
        }
        // WP-B（v3 B-1）：stats.jsonl 落盘协调器——与 last_known 同
        // FlushTuning 容错（tmp→rename 原子写、半截行截断修复）。启动即落
        // run_start 生命周期事件（每次运行 2 行之一，缓冲待节流落盘）。
        // 相位二（第 4 项）：experimental_stats=false 时整体停用——不加载、
        // 不记录、不落盘（store 入口全 no-op）。启动时读取一次，重启生效。
        let stats_enabled = core
            .lock()
            .map(|c| c.config().experimental_stats)
            .unwrap_or(true);
        let (mut stats_store, stats_load) = if stats_enabled {
            volttray_app::stats::StatsStore::load(
                &volttray_app::stats::default_dir(),
                FlushTuning::default(),
            )
        } else {
            bootlog::note(
                "experimental: stats OFF — stats.jsonl disabled (no load, no events, no writes)",
            );
            (
                volttray_app::stats::StatsStore::disabled(&volttray_app::stats::default_dir()),
                volttray_app::stats::StatsLoadOutcome::Empty,
            )
        };
        match stats_load {
            volttray_app::stats::StatsLoadOutcome::Empty => {
                bootlog::note("stats.jsonl: fresh start")
            }
            volttray_app::stats::StatsLoadOutcome::Ready => {
                bootlog::note("stats.jsonl: existing store ready")
            }
            volttray_app::stats::StatsLoadOutcome::Repaired(n) => bootlog::note(&format!(
                "stats.jsonl: truncated {n} trailing byte(s) (crash mid-write repair)"
            )),
        }
        stats_store.note_run_start(wall_secs() as i64, Instant::now());
        // 融合稿 Phase 1：静止/活动分钟位图桶（RawInput 系统事件驱动，
        // 纯内存计数、零新增设备查询——D-33 红线；不落盘高频事件，
        // 仅 boundary（本地日界轮转点）/run_stop 两处并入桶摘要进
        // stats 事件流——融合稿 §5 包数账原文口径（复核批 F3：补齐
        // boundary 冲刷，崩溃损失窗口由「自 run_start 以来」收窄到
        // 「自上一日界以来」）。
        let mut activity_buckets =
            volttray_app::stats::MinuteBuckets::new(crate::export::local_offset_minutes() as i64);
        // F3：上次 mark 所在本地日——跨日时触发 boundary 冲刷。
        let mut last_bucket_date: Option<String> = None;
        // v0.3.10（P3-日志）：上次落日志时的枚举候选集合签名——正常态 30s
        // 一次的例行枚举在集合未变时不重复全量落盘（单会话日志体积上限）。
        let mut last_enum_sig: Option<String> = None;
        // v0.3.11（F3）：无设备态重扫轮次计数（汇总日志用，单会话上限
        // RESCAN_LOG_CAP 行——死锁场景枚举签名不变，签名去重不落日志，
        // 没有这行汇总则「0 槽位 + 死缓存句柄」期间完全无观测）。
        let mut rescan_round: u32 = 0;
        // v0.3.15（P1-1）：唤醒信号只触发「仅探测」轮——标记下一轮
        // （probe_only_next）。WP-C（v3 C-1）：整轮 vs 仅探测轮的裁决
        // 从墙钟到期点（full_poll_due）迁移为「是否有在线设备到期应查」
        // （AppCore::any_due，per-device next_due 口径）——探测轮不刷新
        // 任何设备的到期点；任一在线设备到期即降级回整轮，探测轮不会
        // 饿死各设备自己的轮询节奏。
        let mut probe_only_next = false;
        // R-4（v0.3.17）：attribution miss 日志采样（每分钟 1 条 + 计数）。
        let mut wake_miss_sampler = MissLogSampler::new();
        // BF-2（v0.3.17）＋ E22（报告口径）：零信息裁决日志稳态采样——
        // hit / miss-* ∧ 未装填 ∧ stay-asleep 都是鼠标活动稳态下的高频
        // 无变化行，逐条落盘即刷屏（BF-2 原口径只覆盖 hit 桶，E10/E11
        // 拆桶后 miss-* 各桶的同类行同样 ~2s/条）。按桶稳态采样（每
        // 分钟 1 条+计数）；桶跃迁 / 装填 / probe-round /
        // core-lock-failed 仍逐条全量，跃迁冲出上一桶压制计数不丢数。
        let mut wake_steady_sampler = SteadyDecisionSampler::new();
        'outer: loop {
            // WP-B：循环顶部的 try_recv 原本只识别 Shutdown——非 Shutdown
            // 命令在此被消费即丢弃（WakeSignal 丢弃无害：轮次
            // 即刻开始，语义不变）；ExportStats 丢弃 = 导出静默丢失，必须
            // 在此同样派发执行。v0.3.17（一轮 P0-③）注释更正：RefreshNow
            // 丢弃并非无害——底部处理器出队 WakeSignal 后 continue
            // 'outer，紧随的用户手动刷新会在循环顶此处出队（竞态窗口 =
            // 底部出队到循环顶 try_recv 的微秒缝），旧语义下被静默
            // 丢弃、「立即刷新」失效，现显式匹配转发（见下）。
            match cmd.try_recv() {
                Ok(IoCmd::Shutdown) => {
                    // 小修批第 3 项：会话收尾冲出两个唤醒日志采样器的
                    // pending 压制计数——不冲出则 diag 计数器（per-run
                    // 全量）与采样日志行永不可对平（E10 修复前后计数
                    // 对账前置，口径见 diag_export 字段注释）。
                    if let Some(line) = wake_miss_sampler.flush_tail() {
                        bootlog::note(&line);
                    }
                    if let Some(line) = wake_steady_sampler.leave_steady() {
                        bootlog::note(&line);
                    }
                    // 融合稿 Phase 1：run_stop 并入活动桶摘要（不落盘高频
                    // 事件，仅 boundary/run_stop 把分钟位图并入事件流——
                    // boundary 冲刷见 WakeSignal 处理器；同值快照去重）。
                    let bucket_ts = wall_secs() as i64;
                    for (dev, days) in activity_buckets.take_snapshot() {
                        stats_store.note_activity_buckets(&dev, days, bucket_ts, Instant::now());
                    }
                    // WP-B：run_stop 生命周期事件 + 强制落盘（每次运行 2 行之一）。
                    stats_store.note_run_stop(wall_secs() as i64, Instant::now());
                    if let Err(e) = stats_store.shutdown_flush(Instant::now(), wall_secs() as i64) {
                        bootlog::note(&format!("stats.jsonl shutdown flush failed: {e}"));
                    }
                    // WP-A（v3 A-1）：退出兜底 fsync，崩溃损失 ≤60s / 掉电 ≤5min。
                    if let Err(e) = store.shutdown_flush(Instant::now()) {
                        bootlog::note(&format!("last_known.json shutdown flush failed: {e}"));
                    }
                    // WP-B（I-02）：回执落盘完成（菜单退出 / WM_ENDSESSION 等待方）。
                    let _ = shutdown_ack.send(());
                    return;
                }
                Ok(IoCmd::ExportStats(req)) => {
                    let msg = run_stats_export(&mut stats_store, &req);
                    gui.send(GuiMsg::StatsExportDone(msg));
                }
                // v0.3.17（一轮 P0-③）：循环顶 RefreshNow 显式转发
                // request_refresh——与底部处理器（~1800）同一语义，修复
                // 唤醒裁决后手动刷新被丢弃的竞态。
                Ok(IoCmd::RefreshNow) => {
                    reconnect.request_refresh();
                }
                // WakeSignal：维持旧语义（轮次即刻开始，丢弃无害）。
                _ => {}
            }

            // v0.3.15（P1-1）：本轮是否为「仅探测」轮——仅当无在线设备
            // 到期时成立；任一在线设备到期即强制整轮，保证 per-device
            // 轮询节奏语义不变（WP-C：full_poll_due 墙钟裁决已退役，
            // 其角色由 AppCore::any_due / next_poll_due 吸收）。
            let probe_only = std::mem::take(&mut probe_only_next)
                && match core.lock() {
                    Ok(c) => !c.any_due(Instant::now()),
                    Err(_) => false,
                };
            // 仅探测轮：枚举前已在册的槽位集合——本轮只探测「枚举新注册」
            // 与「仍处休眠」的槽位，已在册在线设备跳过（见轮询循环）。
            let pre_enum_ids: HashSet<String> = if probe_only {
                channels
                    .iter()
                    .flat_map(|c| c.slots.iter().map(|s| s.id.clone()))
                    .collect()
            } else {
                HashSet::new()
            };

            // 相位二（第 4 项）：experimental_bluetooth_wired=false 时跳过
            // HID++ 0xFF 直连槽扫描（蓝牙/有线直连设备不参与发现；接收器
            // 槽位 1..=6 不受影响）。每轮读一次配置，运行期改配置下一轮
            // 即生效（与档位热重载同口径）。
            let bt_wired_scan = core
                .lock()
                .map(|c| c.config().experimental_bluetooth_wired)
                .unwrap_or(false);
            // 重枚举裁决（D-65）：正常态沿用 30s 门控；无设备且连候选通道
            // 都没有时按 30s/60s/120s 退避（仅主机侧枚举、零射频，不违反
            // 零耗电约束）；「立即刷新」/句柄失效置 force → 立即重枚举。
            let enum_now = Instant::now();
            if reconnect.should_enumerate(enum_now, !channels.is_empty()) {
                // v0.3.11（F3）：本轮枚举是否重开过缓存句柄（无设备态
                // 汇总日志用；声明在 match 外供轮末读取）。
                let mut reopened_this_round = false;
                // v0.3.11（P3，质检复审）：scrap 整通道作废的强制重枚举
                // 须延迟到本轮 record_result **之后**置位——record_result
                // 无条件清 force（main.rs 老代码先 invalidate 后 record，
                // force 被同轮清掉，「立即重枚举」退化成下一个 30s 门控点）。
                let mut force_after_record = false;
                // v0.3.12（D-69）：本轮枚举快照素材（诊断导出用）——
                // 候选数 / 各通道 open 结果 / 枚举失败原因 / 是否强制轮。
                let mut enum_channels: Vec<ChannelEnumResult> = Vec::new();
                let mut enum_candidates = 0usize;
                let mut enum_error: Option<String> = None;
                let mut forced_round = false;
                match hidapi::HidApi::new() {
                    Ok(api) => {
                        // v0.3.10（P2）：本轮枚举是否由「立即刷新」/句柄失效
                        // 强制触发——强制轮对已有通道一并重开句柄（通道 key
                        // 不变不代表缓存句柄还活着：接收器同口重插/驱动重载
                        // 后旧句柄已死，不重开则刷新仍打在死句柄上）。
                        let forced = reconnect.pending_force();
                        forced_round = forced;
                        let found = enumerate_channels(&api);
                        enum_candidates = found.len();
                        // v0.3.10（P3-日志）：候选集合签名未变则例行枚举不落
                        // 日志——仅集合变化（或下方 Err 臂的枚举失败）时记录，
                        // 与「只记状态变化与失败」口径一致。
                        let enum_sig = {
                            let mut parts: Vec<String> = found.iter().map(|i| i.key()).collect();
                            parts.sort();
                            parts.join(";")
                        };
                        if last_enum_sig.as_deref() != Some(enum_sig.as_str()) {
                            // v0.3.9（A）：枚举结果落设备层日志（候选通道数 +
                            // 路径/VID/PID），重启后「找不到设备」直接可见。
                            bootlog::note(&format!(
                                "hid enum: {} hidpp candidate channel(s)",
                                found.len()
                            ));
                            for info in &found {
                                let short = info
                                    .short_path
                                    .as_deref()
                                    .map(|p| p.to_string_lossy().into_owned())
                                    .unwrap_or_default();
                                let long = info
                                    .long_path
                                    .as_deref()
                                    .map(|p| p.to_string_lossy().into_owned())
                                    .unwrap_or_default();
                                bootlog::note(&format!(
                                    "hid enum candidate: vid=0x{:04X} pid=0x{:04X} tag={} product={} short={} long={}",
                                    volttray_protocol::LOGITECH_VID,
                                    info.pid,
                                    info.tag,
                                    info.product,
                                    short,
                                    long
                                ));
                            }
                            last_enum_sig = Some(enum_sig);
                        }
                        let mut seen: Vec<String> = Vec::new();
                        // v0.3.11（F2）：「扫槽连败重开失败、待整通道作废」的
                        // 通道 key（借用期内不能就地移除，循环外统一处理）。
                        let mut scrapped_channels: Vec<String> = Vec::new();
                        // T6：rebind 的旧端点 id（借用约束——逐通道扫描期间
                        // 不能跨通道改槽位，循环外统一标记为别名）。
                        let mut pending_aliases: Vec<String> = Vec::new();
                        for info in found {
                            let key = info.key();
                            seen.push(key.clone());
                            if let Some(state) = channels.iter_mut().find(|c| c.key == key) {
                                // v0.3.12（D-69）：已有通道本轮 open 状态
                                // （cached / reopened / cached-reopen-failed），
                                // 供诊断导出快照。
                                let mut open_status = "cached".to_string();
                                let mut open_error: Option<String> = None;
                                // v0.3.10（P2）：强制枚举轮重开已有通道句柄——
                                // 「立即刷新」第一次点击即生效（v0.3.9 只在
                                // 死句柄上补扫槽位，需 3 连败才自愈）。
                                if forced {
                                    match HidDeviceChannel::open(
                                        &api,
                                        state.info.short_path.as_deref(),
                                        state.info.long_path.as_deref(),
                                    ) {
                                        Ok(ch) => {
                                            bootlog::note(&format!(
                                                "hid reopen ok (forced enum): pid=0x{:04X} tag={}",
                                                state.info.pid, state.info.tag
                                            ));
                                            state.shared = SharedChannel(
                                                Arc::new(Mutex::new(ch)),
                                                telemetry.frames.clone(),
                                            );
                                            // 新句柄重新裁决活性；槽位连败清零。
                                            state.liveness = ChannelLiveness::new();
                                            // v0.3.11（F2/F3）：扫槽连败同步清零；
                                            // 本轮汇总日志标记重开过句柄。
                                            state.scan_streak = SlotScanStreak::new();
                                            // R-1/R-2（v0.3.17）：句柄重开 = 新的
                                            // 一轮观测，0xFF 负缓存与死槽冷却同步
                                            // 清零（force 枚举 = 手动刷新/动鼠标
                                            // 立即穿透降频）。
                                            state.ff_battery_probe_failed = false;
                                            state.dead_slots.clear();
                                            reopened_this_round = true;
                                            for s in &mut state.slots {
                                                s.hard_failures = 0;
                                            }
                                            open_status = "reopened".to_string();
                                        }
                                        Err(e) => {
                                            // 重开失败保留缓存句柄：活性裁决
                                            // 会在后续轮次判死并触发失效自愈。
                                            bootlog::note(&format!(
                                                "hid reopen failed (forced enum): pid=0x{:04X} tag={}: {e}; keep cached handle",
                                                state.info.pid, state.info.tag
                                            ));
                                            open_status = "cached-reopen-failed".to_string();
                                            open_error = Some(e.to_string());
                                        }
                                    }
                                }
                                // 已有通道：补扫新出现的槽位（休眠唤醒 / 新配对）。
                                // v0.3.11（F2）：扫槽结果按轮收集，轮末交
                                // SlotScanStreak 裁决——连续 SLOT_SCAN_REOPEN_THRESHOLD
                                // 轮「全部失败且至少一次 I/O 类」才强制重开句柄
                                // （跨槽位去重、按轮计；纯超时轮清零——设备深睡
                                // 不误判成句柄死亡，P1 同族陷阱）。
                                let mut scanned = 0usize;
                                let mut scan_failures: Vec<SlotScanFailure> = Vec::new();
                                for slot in SCAN_SLOTS {
                                    // 相位二（第 4 项）：0xFF 直连槽按实验
                                    // 开关门控（关闭 = 不扫蓝牙/有线直连）。
                                    if slot == 0xFF && !bt_wired_scan {
                                        continue;
                                    }
                                    if state.slots.iter().any(|s| s.index == slot) {
                                        continue;
                                    }
                                    // R-2（v0.3.17）：死槽冷却——连续 K 轮握手
                                    // 无设备应答（Timeout 或 NoDevice，E10 细
                                    // 分口径）的槽位每 K 轮才扫 1 次（降功
                                    // 耗/降噪）；被跳过的槽位不计入本轮
                                    // scanned，也不喂 SlotScanStreak（未观测
                                    // 不得裁决）；force 枚举已清零，立即穿透。
                                    if !state.dead_slots.should_scan(slot) {
                                        // 跳过轮同样按「无应答」推进连击——
                                        // 否则 streak 冻结在 K+1、冷却槽永不
                                        // 复扫（BF-1）；推进后 streak 逢 K 的
                                        // 整数倍放行 1 次，「每 K 轮扫 1 次」
                                        // 与自动发现 ≤5 分钟的披露才成立。
                                        state.dead_slots.note_handshake(slot, true);
                                        continue;
                                    }
                                    scanned += 1;
                                    let mut edge = SlotScanEdge {
                                        ff_battery_probe_failed: state.ff_battery_probe_failed,
                                        ..SlotScanEdge::default()
                                    };
                                    let outcome = try_slot(
                                        &core,
                                        &observer,
                                        &state.shared,
                                        &state.info,
                                        slot,
                                        &mut edge,
                                        &mut stats_store,
                                    );
                                    // R-1/R-2：边效应回写通道状态（edge 携
                                    // 最终值——HID++1 握手成功时 try_slot
                                    // 内已清缓存，故整体赋值而非 |=）。
                                    state.ff_battery_probe_failed = edge.ff_battery_probe_failed;
                                    state
                                        .dead_slots
                                        .note_handshake(slot, edge.handshake_no_response);
                                    match outcome {
                                        Ok((s, rebound)) => {
                                            // E22：唤醒信号窗口内重枚举捞回
                                            // 休眠/断开设备 → 补记遥测。
                                            note_wake_recovery_via_reenum(
                                                &telemetry,
                                                edge.recovered_via_scan,
                                                last_wake_signal_at,
                                                &s.id,
                                                Instant::now(),
                                            );
                                            // WP-A：扫槽成功读数写入持久化
                                            // （change-driven dirty，节流在
                                            // store 内；虚拟/冲突键内部拒绝）。
                                            if let Ok(c) = core.lock() {
                                                let _ = c.persist_note_success(
                                                    &mut store,
                                                    &s.id,
                                                    wall_secs(),
                                                    Instant::now(),
                                                );
                                            }
                                            // FR-1（RB2）：注册期落 raw path →
                                            // unitId 持久映射（重启空槽位窗口
                                            // 的唤醒桶键兜底；变更驱动、随
                                            // 节拍落盘）。
                                            if let Some(uid) = s.unit_id.as_deref() {
                                                for p in
                                                    [&state.info.short_path, &state.info.long_path]
                                                        .into_iter()
                                                        .flatten()
                                                {
                                                    store.note_path_map(&p.to_string_lossy(), uid);
                                                }
                                            }
                                            state.slots.push(s);
                                            if let Some((old_id, _)) = rebound {
                                                pending_aliases.push(old_id);
                                            }
                                        }
                                        Err(f) => scan_failures.push(f),
                                    }
                                }
                                if state.scan_streak.note_round(scanned, &scan_failures) {
                                    // 连败达阈值：缓存句柄可疑，强制重开——
                                    // 主机侧 HID open，零射频，不触碰任何
                                    // 休眠设备（零鼠标电池红线）。
                                    match HidDeviceChannel::open(
                                        &api,
                                        state.info.short_path.as_deref(),
                                        state.info.long_path.as_deref(),
                                    ) {
                                        Ok(ch) => {
                                            bootlog::note(&format!(
                                                "hid reopen ok (scan-streak {} rounds): pid=0x{:04X} tag={}",
                                                SLOT_SCAN_REOPEN_THRESHOLD,
                                                state.info.pid,
                                                state.info.tag
                                            ));
                                            state.shared = SharedChannel(
                                                Arc::new(Mutex::new(ch)),
                                                telemetry.frames.clone(),
                                            );
                                            // 新句柄重新裁决活性；各类连败清零。
                                            state.liveness = ChannelLiveness::new();
                                            for s in &mut state.slots {
                                                s.hard_failures = 0;
                                            }
                                            state.scan_streak.note_reopen_result(true);
                                            reopened_this_round = true;
                                            open_status = "reopened".to_string();
                                            open_error = None;
                                        }
                                        Err(e) => {
                                            // 重开失败 = 句柄重开无望：整通道
                                            // 作废（循环外统一移除）+ 强制全量
                                            // 重枚举。
                                            bootlog::note(&format!(
                                                "hid reopen failed (scan-streak {} rounds): pid=0x{:04X} tag={}: {e}; scrap channel + re-enumerate",
                                                SLOT_SCAN_REOPEN_THRESHOLD,
                                                state.info.pid,
                                                state.info.tag
                                            ));
                                            open_status = "cached-reopen-failed".to_string();
                                            open_error = Some(e.to_string());
                                            if state.scan_streak.note_reopen_result(false) {
                                                scrapped_channels.push(state.key.clone());
                                            }
                                        }
                                    }
                                }
                                // v0.3.12（D-69）：记入本轮枚举快照。
                                enum_channels.push(ChannelEnumResult {
                                    key: state.key.clone(),
                                    pid: format!("0x{:04X}", state.info.pid),
                                    tag: state.info.tag.clone(),
                                    open: open_status,
                                    error: open_error,
                                });
                                continue;
                            }
                            // v0.3.9（A）：通道打开成功/失败（含错误原因）落日志。
                            let ch = match HidDeviceChannel::open(
                                &api,
                                info.short_path.as_deref(),
                                info.long_path.as_deref(),
                            ) {
                                Ok(ch) => ch,
                                Err(e) => {
                                    bootlog::note(&format!(
                                        "hid open failed: pid=0x{:04X} tag={}: {e}",
                                        info.pid, info.tag
                                    ));
                                    // v0.3.12（D-69）：打开失败也记入快照。
                                    enum_channels.push(ChannelEnumResult {
                                        key: key.clone(),
                                        pid: format!("0x{:04X}", info.pid),
                                        tag: info.tag.clone(),
                                        open: "failed".to_string(),
                                        error: Some(e.to_string()),
                                    });
                                    continue;
                                }
                            };
                            bootlog::note(&format!(
                                "hid open ok: pid=0x{:04X} tag={}",
                                info.pid, info.tag
                            ));
                            // v0.3.12（D-69）：新通道本轮打开成功。
                            enum_channels.push(ChannelEnumResult {
                                key: key.clone(),
                                pid: format!("0x{:04X}", info.pid),
                                tag: info.tag.clone(),
                                open: "ok".to_string(),
                                error: None,
                            });
                            let shared =
                                SharedChannel(Arc::new(Mutex::new(ch)), telemetry.frames.clone());
                            let mut slots = Vec::new();
                            // R-1/R-2（v0.3.17）：新通道首轮全量扫描，边效应
                            // 逐槽累积后直接写入新通道状态（首轮无冷却——
                            // dead_slots 从零计起）。
                            let mut ff_battery_probe_failed = false;
                            let mut dead_slots = DeadSlotScan::new();
                            for slot in SCAN_SLOTS {
                                // 相位二（第 4 项）：0xFF 直连槽按实验开
                                // 关门控（关闭 = 不扫蓝牙/有线直连）。
                                if slot == 0xFF && !bt_wired_scan {
                                    continue;
                                }
                                let mut edge = SlotScanEdge {
                                    ff_battery_probe_failed,
                                    ..SlotScanEdge::default()
                                };
                                let outcome = try_slot(
                                    &core,
                                    &observer,
                                    &shared,
                                    &info,
                                    slot,
                                    &mut edge,
                                    &mut stats_store,
                                );
                                ff_battery_probe_failed = edge.ff_battery_probe_failed;
                                dead_slots.note_handshake(slot, edge.handshake_no_response);
                                if let Ok((s, rebound)) = outcome {
                                    // E22：唤醒信号窗口内重枚举捞回
                                    // 休眠/断开设备 → 补记遥测。
                                    note_wake_recovery_via_reenum(
                                        &telemetry,
                                        edge.recovered_via_scan,
                                        last_wake_signal_at,
                                        &s.id,
                                        Instant::now(),
                                    );
                                    // FR-1（RB2）：同上——新通道首轮扫槽
                                    // 注册的 raw path → unitId 映射。
                                    if let Some(uid) = s.unit_id.as_deref() {
                                        for p in [&info.short_path, &info.long_path]
                                            .into_iter()
                                            .flatten()
                                        {
                                            store.note_path_map(&p.to_string_lossy(), uid);
                                        }
                                    }
                                    slots.push(s);
                                    if let Some((old_id, _)) = rebound {
                                        pending_aliases.push(old_id);
                                    }
                                }
                            }
                            channels.push(ChannelState {
                                key,
                                info,
                                shared,
                                liveness: ChannelLiveness::new(),
                                scan_streak: SlotScanStreak::new(),
                                ff_battery_probe_failed,
                                dead_slots,
                                slots,
                            });
                        }
                        // T6：rebind 旧端点统一标记为别名（跳过轮询 /
                        // 不进菜单 / 不 toast；仍留在 slots 中避免重扫抖动）。
                        if !pending_aliases.is_empty() {
                            for c in &mut channels {
                                for s in &mut c.slots {
                                    if pending_aliases.contains(&s.id) {
                                        s.alias = true;
                                    }
                                }
                            }
                        }
                        // 掉线通道整体移除（接收器拔出）。
                        let mut i = 0;
                        while i < channels.len() {
                            if !seen.contains(&channels[i].key) {
                                let gone = channels.remove(i);
                                for s in &gone.slots {
                                    // T6：别名槽位从未在台账中独立存在，
                                    // 随通道消失即可，不动台账。
                                    if s.alias {
                                        bootlog::note(&format!(
                                            "alias slot gone with channel: id={}",
                                            s.id
                                        ));
                                        continue;
                                    }
                                    // T6 拔线晋级：主端点所在通道消失时，从
                                    // 仍存活通道里挑同 unitId 的别名晋级为
                                    // 新主端点（无线回落身份连续；电量 /
                                    // 低电追踪 / pin 选定不丢）。
                                    let promoted = match core.lock() {
                                        Ok(mut c) => {
                                            try_promote(&mut c, &observer, &mut channels, s, &|k| {
                                                seen.iter().any(|x| x == k)
                                            })
                                        }
                                        Err(_) => false,
                                    };
                                    if promoted {
                                        continue;
                                    }
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
                        // v0.3.11（F2）：扫槽连败且重开失败的通道整通道作废
                        // （句柄重开无望），剩余槽位置为离线，强制全量重枚举
                        // （主机侧操作，零射频）。
                        if !scrapped_channels.is_empty() {
                            let mut removed_slots: Vec<SlotState> = Vec::new();
                            channels.retain_mut(|c| {
                                if scrapped_channels.contains(&c.key) {
                                    removed_slots.append(&mut c.slots);
                                    false
                                } else {
                                    true
                                }
                            });
                            for s in &removed_slots {
                                // T6：别名不在台账；主端点先尝试向存活
                                // 通道的别名晋级，无候选才置离线。
                                if s.alias {
                                    continue;
                                }
                                let promoted = match core.lock() {
                                    Ok(mut c) => {
                                        try_promote(&mut c, &observer, &mut channels, s, &|_| true)
                                    }
                                    Err(_) => false,
                                };
                                if promoted {
                                    continue;
                                }
                                if let Ok(mut c) = core.lock() {
                                    c.record_failure(&s.id, true);
                                }
                                if let Ok(mut o) = observer.lock() {
                                    o.push(DiagEvent::new(
                                        Layer::App,
                                        DiagKind::DeviceLost { id: s.id.clone() },
                                    ));
                                }
                            }
                            // v0.3.11（P3）：不再在此处 invalidate——移到本轮
                            // record_result 之后（见 force_after_record 声明处）。
                            force_after_record = true;
                        }
                    }
                    Err(e) => {
                        bootlog::note(&format!("hid enum failed: HidApi init error: {e}"));
                        // v0.3.12（D-69）：枚举本身失败也更新快照（error 字段
                        // 说明原因，候选数 0、通道明细空）。
                        enum_error = Some(format!("HidApi init failed: {e}"));
                    }
                }
                // 进入/退出「无设备」态：事件级日志（诊断流 + bootlog 双写）。
                let total_slots: usize = channels.iter().map(|c| c.slots.len()).sum();
                // v0.3.12（D-69）：本轮枚举快照写入共享内存（GUI 线程导出诊断
                // 时读取）。锁中毒则本轮跳过（下次枚举再写），绝不影响主流程。
                if let Ok(mut snap) = enum_snap.lock() {
                    *snap = Some(EnumSnapshot {
                        at_ms: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0),
                        forced: forced_round,
                        candidate_channels: enum_candidates,
                        channels: enum_channels,
                        registered_slots: total_slots,
                        error: enum_error,
                    });
                }
                match reconnect.record_result(enum_now, total_slots) {
                    Some(EnumTransition::EnteredNoDevice) => {
                        bootlog::note(
                            "no-device: entered (0 registered slot after enum); \
                             backoff enum retry 30s/60s/120s (host-side only, zero rf)",
                        );
                        if let Ok(mut o) = observer.lock() {
                            o.push(DiagEvent::new(
                                Layer::App,
                                DiagKind::Info {
                                    message: "no-device: entered; enum backoff 30s/60s/120s".into(),
                                },
                            ));
                        }
                    }
                    Some(EnumTransition::ExitedNoDevice) => {
                        bootlog::note(
                            "no-device: exited (device registered); resume normal polling",
                        );
                        if let Ok(mut o) = observer.lock() {
                            o.push(DiagEvent::new(
                                Layer::App,
                                DiagKind::Info {
                                    message: "no-device: exited; resume normal polling".into(),
                                },
                            ));
                        }
                    }
                    None => {}
                }
                // v0.3.11（P3）：本轮有整通道作废（扫槽连败且重开失败）→
                // 在 record_result 消费掉旧 force 之后置位，下一轮循环
                // 立即全量重枚举（不再退化到下一个 30s 门控点）。
                if force_after_record {
                    reconnect.invalidate_handles();
                }
                // v0.3.11（F3）：无设备态每轮重扫落一行汇总日志。与枚举签名
                // 去重独立（死锁场景候选集合不变、签名去重不落盘，没有这行
                // 则「0 槽位 + 死缓存句柄」期间完全无观测）；单会话上限
                // RESCAN_LOG_CAP 行防爆量。
                if reconnect.no_device() {
                    rescan_round = rescan_round.saturating_add(1);
                    if rescan_round <= RESCAN_LOG_CAP {
                        bootlog::note(&format!(
                            "rescan round {}: channels={} slots={} cached_handle reopen={}",
                            rescan_round,
                            channels.len(),
                            total_slots,
                            if reopened_this_round { "yes" } else { "no" }
                        ));
                        if rescan_round == RESCAN_LOG_CAP {
                            bootlog::note("rescan summary log capped for this session");
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
                // D-67：本轮判定为「句柄已死」的通道 key，循环外统一移除并
                // 触发重枚举（移除期间不能就地改 channels，先收集）。
                let mut dead_channels: Vec<String> = Vec::new();
                // T6：轮询中注销的槽位（DeviceGone / 硬失败 3 连败）——
                // DeviceLost 事件延后到「向存活别名晋级」裁决之后发放
                // （借用约束：逐通道轮询期间不能跨通道改槽位）。
                let mut pending_removed_slots: Vec<SlotState> = Vec::new();
                for ch_state in &mut channels {
                    // D-67（P1）：本轮查询结果按槽位分类收集，轮末交
                    // ChannelLiveness 统一裁决——整轮零应答才记一次连击，
                    // 不再按「次」跨槽位累计（多设备同睡不误判死）。
                    let mut normal_outcomes: Vec<SlotOutcome> = Vec::new();
                    let mut probe_outcomes: Vec<SlotOutcome> = Vec::new();
                    let mut i = 0;
                    while i < ch_state.slots.len() {
                        let s = &mut ch_state.slots[i];
                        // T6：别名槽位不参与轮询——同一物理设备的重复端点，
                        // 零查询、不喂活性裁决（主端点已覆盖该设备）。
                        if s.alias {
                            i += 1;
                            continue;
                        }
                        // v0.3.15（P1-1）：仅探测轮跳过「已在册且已唤醒」的
                        // 在线设备——它们按正常轮询周期查询，不被鼠标活动
                        // 拖着高频查（原缺陷：WakeSignal 每 2s 整轮重查，
                        // 持续动鼠标时在线设备被以 ~0.5Hz 查询电量，是
                        // 1800s 档位的 900 倍）。休眠设备（含本轮新装填
                        // 唤醒资格的）与枚举新注册槽位仍照常探测。
                        // F5 主修复 (b)：跳过判定从「仅唤醒候选」放宽为
                        // 「唤醒候选 ∨ 临近灰显豁免」——在线设备距 90s 无
                        // 成功不足 30s 时放行一次查询，活跃期读数间隔收敛
                        // 到 ~90s 量级，不再冻结到下一次整轮（≥ 轮询档）
                        // 后被 90s 墙钟派生误灰、误报离线。
                        if probe_only
                            && pre_enum_ids.contains(&s.id)
                            && !core.probe_round_allows(&s.id, now)
                        {
                            i += 1;
                            continue;
                        }
                        // 休眠判定提前（WP-C 到期门与唤醒探测都要用）。
                        let sleeping = core.entries().iter().any(|e| {
                            e.device.id == s.id && e.device.state == DeviceState::Sleeping
                        });
                        // WP-C（v3 C-1）：整轮按设备到期过滤——未到期的
                        // 在线设备本轮不查（每台设备自己的 next_due 决定
                        // 自己的查询节奏）；休眠设备不受本门约束，交由
                        // poll_decision 裁决唤醒资格 / 冷却。
                        if !probe_only && !sleeping && !core.is_due(&s.id, now) {
                            i += 1;
                            continue;
                        }
                        // 休眠且无唤醒资格：本轮绝不向该设备发任何 HID++ 查询。
                        if core.poll_decision(&s.id, now) == PollDecision::Skip {
                            i += 1;
                            continue;
                        }
                        // R-3（v0.3.17）：已装填唤醒资格的在线灰显设备按
                        // 唤醒探测口径处理（静默进 probe_outcomes 放宽判死、
                        // 成功计唤醒成功数）。仅探测轮存在此态；整轮里在线
                        // 设备走自己的到期门（装填已把到期点提前）。
                        let armed_gray =
                            probe_only && !sleeping && core.wake_armed_gray(&s.id, now);
                        // 唤醒探测开始：消耗唤醒资格并进入冷却（D-33）。
                        if sleeping || armed_gray {
                            core.begin_wake_probe(&s.id, now);
                        }
                        let mut t = Transport::new(
                            ch_state.shared.clone(),
                            SharedObserver(observer.clone()),
                        );
                        let reading =
                            probe_battery(&mut t, &s.id, s.index, &s.features, CURVE, s.is_hidpp1);
                        // D-67：唤醒探测与普通轮询的静默分开计数——已知休眠
                        // 的设备探测静默是常态，判死口径放宽（P3-唤醒探测）。
                        // R-3：装填灰显设备的探测同口径进 probe_outcomes。
                        let outcomes = if sleeping || armed_gray {
                            &mut probe_outcomes
                        } else {
                            &mut normal_outcomes
                        };
                        match reading {
                            Ok(r) => {
                                s.hard_failures = 0;
                                outcomes.push(SlotOutcome::Success);
                                // R-3/R-11（v0.3.17）：唤醒探测（休眠或装填
                                // 灰显）成功计数，供诊断导出 summary。
                                if sleeping || armed_gray {
                                    telemetry.wake_successes.fetch_add(1, Ordering::Relaxed);
                                }
                                // WP-A：轮询成功读数写入持久化（同上节流）。
                                let _ =
                                    core.persist_note_success(&mut store, &s.id, wall_secs(), now);
                                // WP-B：成功读数落统计事件（变化驱动过滤在
                                // store 内；pre-read 休眠 → wake-first-read
                                // 必记——休眠段终点锚点）。physical_key 缺失
                                // 时跳过（身份不明键不参与归因，与持久化同
                                // 口径）。
                                {
                                    let (pct, chg) = (r.percent, r.charging);
                                    let (pk, dn) = core
                                        .entries()
                                        .iter()
                                        .find(|e| e.device.id == s.id)
                                        .map(|e| {
                                            (
                                                e.device.physical_key.as_str().to_string(),
                                                e.device.name.clone(),
                                            )
                                        })
                                        .unwrap_or_default();
                                    if !pk.is_empty() {
                                        stats_store.note_reading(
                                            &pk,
                                            &dn,
                                            pct,
                                            chg,
                                            volttray_app::stats::read_source(sleeping, chg),
                                            wall_secs() as i64,
                                            now,
                                        );
                                    }
                                }
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
                                // E22：HID++1 0x8F/0x08（UNKNOWN_DEVICE，接收器
                                // 应答但槽位无设备）语义等价超时，并入同一休眠路径
                                // ——通道活性仍记 Responded（接收器回了字节）。
                                // 仅限已注册槽位的轮询路径；扫描期 0x08 的 E10 死槽
                                // 冷却映射不受影响（互斥阶段，见 battery.rs）。
                                // D-67：通道级判死改由轮末 ChannelLiveness 按
                                // 「整轮零应答」裁决，此处只分类登记结果。
                                UnknownReason::QueryTimeout | UnknownReason::NoDeviceAtSlot => {
                                    outcomes.push(match reason {
                                        UnknownReason::NoDeviceAtSlot => SlotOutcome::Responded,
                                        _ => SlotOutcome::Timeout,
                                    });
                                    bootlog::note(&format!(
                                        "battery query failed id={} reason={} (sleep path)",
                                        s.id,
                                        reason.code()
                                    ));
                                    if core.record_timeout(&s.id, now)
                                        == TimeoutOutcome::EnteredSleep
                                    {
                                        // WP-B：sleep_enter 事件（分段归因
                                        // 唯一起点；判定本就发生、仅多落一
                                        // 行日志，零新增设备查询——P0-1）。
                                        {
                                            let pk = core
                                                .entries()
                                                .iter()
                                                .find(|e| e.device.id == s.id)
                                                .map(|e| e.device.physical_key.as_str().to_string())
                                                .unwrap_or_default();
                                            if !pk.is_empty() {
                                                stats_store.note_sleep_enter(
                                                    &pk,
                                                    wall_secs() as i64,
                                                    now,
                                                );
                                            }
                                        }
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
                                // 接收器回了字节 = 通道句柄活着（Responded）。
                                UnknownReason::DeviceGone => {
                                    outcomes.push(SlotOutcome::Responded);
                                    bootlog::note(&format!(
                                        "battery query failed id={} reason=device_gone (deregistered immediately)",
                                        s.id
                                    ));
                                    let gone_slot = ch_state.slots.remove(i);
                                    // T6：DeviceLost 与失败落态延后——先尝试
                                    // 向存活别名晋级（循环外统一裁决；
                                    // 红蓝 P2-1：先晋级、后落失败态）。
                                    pending_removed_slots.push(gone_slot);
                                }
                                // 其它持续失败：设备级错误（错误帧/映射/特性
                                // 缺失等）3 连败只注销本槽位（D-67 P3-1：恢复
                                // v0.3.8 语义，不再误伤同通道健康槽位）；
                                // io_error 是句柄级证据，另喂轮末活性裁决，
                                // 由它按轮决定是否整通道作废。
                                _ => {
                                    if matches!(reason, UnknownReason::IoFailure) {
                                        outcomes.push(SlotOutcome::IoError);
                                    } else {
                                        // 设备回了字节但给不出电量 = 句柄活着。
                                        outcomes.push(SlotOutcome::Responded);
                                    }
                                    s.hard_failures += 1;
                                    bootlog::note(&format!(
                                        "battery query failed id={} reason={} (hard failure {}/{})",
                                        s.id,
                                        reason.code(),
                                        s.hard_failures,
                                        HANDLE_INVALIDATE_THRESHOLD
                                    ));
                                    let gone = s.hard_failures >= HANDLE_INVALIDATE_THRESHOLD;
                                    // 红蓝 P2-1：注销槽位的失败落态延后到
                                    // 晋级裁决之后（先晋级、后落失败态）；
                                    // 未达阈值时 record_failure(gone=false)
                                    // 本就是无操作（core 仅在 gone 且未
                                    // Disconnected 时落态），不再调用。
                                    if gone {
                                        bootlog::note(&format!(
                                            "slot deregistered: id={} hard failures reached {}; rescan on next enum",
                                            s.id, HANDLE_INVALIDATE_THRESHOLD
                                        ));
                                        let gone_slot = ch_state.slots.remove(i);
                                        // T6：DeviceLost 延后——先尝试向存活
                                        // 别名晋级（循环外统一裁决）。
                                        pending_removed_slots.push(gone_slot);
                                    } else {
                                        // WP-C（v3 C-1）：硬失败未达注销
                                        // 阈值——按本设备当前档位重排到期
                                        // 点，防止到期滞留形成对故障设备
                                        // 的紧循环查询。
                                        core.reschedule(&s.id, now);
                                        i += 1;
                                    }
                                }
                            },
                        }
                    }
                    // D-67 轮末裁决：整轮零应答才累计连击；普通静默连击达
                    // HANDLE_INVALIDATE_THRESHOLD 轮、或唤醒探测静默连击达
                    // WAKE_PROBE_INVALIDATE_THRESHOLD 轮才判句柄死。
                    if (!normal_outcomes.is_empty() || !probe_outcomes.is_empty())
                        && ch_state
                            .liveness
                            .note_round(&normal_outcomes, &probe_outcomes)
                    {
                        bootlog::note(&format!(
                            "channel handle invalid: key={} silent rounds {} >= {} or probe silent rounds {} >= {}; invalidate + re-enumerate",
                            ch_state.key,
                            ch_state.liveness.silent_rounds(),
                            HANDLE_INVALIDATE_THRESHOLD,
                            ch_state.liveness.probe_silent_rounds(),
                            WAKE_PROBE_INVALIDATE_THRESHOLD
                        ));
                        dead_channels.push(ch_state.key.clone());
                    }
                }
                // T6：轮询中注销槽位的晋级裁决——主端点被注销（DeviceGone /
                // 硬失败）时先尝试向存活通道的同 unitId 别名晋级；无候选
                // 才落失败态 + 发 DeviceLost（红蓝 P2-1：四条移除路径统一为
                // 「先晋级、后落失败态」——此前本路径先 record_failure 置
                // Disconnected 再晋级，同一拔线事件被 poll 先探测时会残留
                // 离线显示直到下一轮成功轮询，与枚举先探测的「全程在线」
                // 构成两种用户可见终态）。
                for s in &pending_removed_slots {
                    if s.alias {
                        continue;
                    }
                    if try_promote(&mut core, &observer, &mut channels, s, &|_| true) {
                        continue;
                    }
                    core.record_failure(&s.id, true);
                    if let Ok(mut o) = observer.lock() {
                        o.push(DiagEvent::new(
                            Layer::App,
                            DiagKind::DeviceLost { id: s.id.clone() },
                        ));
                    }
                }
                // D-65：句柄失效自愈——整条通道（句柄 + 全部槽位缓存）作废，
                // 剩余槽位置为离线，然后强制下一轮重枚举（主机侧操作，零射频）。
                if !dead_channels.is_empty() {
                    let mut removed_slots: Vec<SlotState> = Vec::new();
                    channels.retain_mut(|c| {
                        if dead_channels.contains(&c.key) {
                            removed_slots.append(&mut c.slots);
                            false
                        } else {
                            true
                        }
                    });
                    for s in &removed_slots {
                        // T6：别名不在台账；主端点先尝试向存活别名晋级。
                        if s.alias {
                            continue;
                        }
                        if try_promote(&mut core, &observer, &mut channels, s, &|_| true) {
                            continue;
                        }
                        core.record_failure(&s.id, true);
                        if let Ok(mut o) = observer.lock() {
                            o.push(DiagEvent::new(
                                Layer::App,
                                DiagKind::DeviceLost { id: s.id.clone() },
                            ));
                        }
                    }
                    reconnect.invalidate_handles();
                }
                for id in core.newly_gray(now) {
                    // F5 主修复第 3 件：原文案「设备离线 / 超过 90 秒未响应」
                    // 在本路径下是虚假陈述（长轮询档下设备从未被查询，谈不上
                    // 「未响应」），且灰显 ≠ 离线。改为「电量显示已过期」
                    // 口径：灰显最后一次已知电量，读数随下轮查询恢复。
                    alerts.push((
                        "LogiVoltTray 电量显示已过期".into(),
                        format!("设备 {id} 的电量读数已过期，已灰显最后一次已知电量"),
                    ));
                }
                let snap = core.snapshot(now);
                gui.send(GuiMsg::Snapshot(Box::new(snap)));
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
                // v0.3.14（红蓝共识 B9 必修）：toast 派发/抑制/失败全部
                // 落 bootlog（此前失败静默吞掉，真机无从确认通知是否到达）。
                if notify_enabled {
                    match toast::notify(&title, &body, silent) {
                        Ok(()) => bootlog::note(&format!("toast dispatched: {title}")),
                        Err(e) => {
                            bootlog::note(&format!("toast dispatch failed title={title}: {e}"))
                        }
                    }
                } else {
                    bootlog::note(&format!(
                        "toast suppressed (notifications disabled): {title}"
                    ));
                }
                gui.send(GuiMsg::Alert);
            }

            // 配置热重载（P1-2：所有权在 app 层）。每轮 io round 检查一次；
            // 分片睡眠内还会按 ~5s 节奏补查（见下），保证 TOML 手改秒级生效。
            handle_config_reload(&mut watcher, &core, &observer, &mut last_reload_warnings);

            // WP-A（v3 R5 I-01 键冲突条款）：每轮同步冲突集——冲突键停写
            // 并移除既有持久化记录与对应虚拟条目（不展示）；LRU 逐出同步
            // 删除虚拟条目。落盘由 store 节流（change-driven + 60s 兜底）。
            if let Ok(mut c) = core.lock() {
                let newly = c.refresh_key_conflicts();
                if !newly.is_empty() {
                    bootlog::note(&format!(
                        "key conflict: fallback key shared by HID++1 devices [{}]; \
                         excluded from persistence & display",
                        newly.join(", ")
                    ));
                }
                let dropped = store.set_conflicted(c.conflicted_keys().clone());
                if !dropped.is_empty() {
                    c.remove_virtual_by_keys(&dropped);
                }
                // WP-B（v3 R5 I-01 同口径）：冲突键同步停写统计事件。
                stats_store.set_conflicted(c.conflicted_keys().clone());
            }
            match store.flush_if_due(Instant::now()) {
                Ok(out) => {
                    if !out.evicted.is_empty() {
                        if let Ok(mut c) = core.lock() {
                            c.remove_virtual_by_keys(&out.evicted);
                        }
                    }
                }
                Err(e) => bootlog::note(&format!("last_known.json flush failed: {e}")),
            }
            // WP-B：stats.jsonl 节流落盘（change-driven + 60s 兜底同
            // FlushTuning）+ 轮转守卫提示（v3：「统计历史即将轮转，建议先
            // 导出」——水位预提示：current 预计越过 9MB 水位而未触顶时
            // 提前一拍发出，此时旧归档仍完好、用户可一键导出；质检 F2
            // 修复前为先覆盖旧归档后弹提示，时序与 v3 口径倒置）。
            if let Err(e) = stats_store.flush_if_due(Instant::now(), wall_secs() as i64) {
                bootlog::note(&format!("stats.jsonl flush failed: {e}"));
            }
            if stats_store.take_rotation_notice() {
                bootlog::note(
                    "stats.jsonl approaching rotation watermark; prompting user to export",
                );
                let (title, body) = (
                    "LogiVoltTray 耗电统计".to_string(),
                    "统计历史即将轮转，建议先导出".to_string(),
                );
                if notify_enabled {
                    match toast::notify(&title, &body, silent) {
                        Ok(()) => bootlog::note("toast dispatched: stats rotation notice"),
                        Err(e) => bootlog::note(&format!("stats rotation toast failed: {e}")),
                    }
                } else {
                    bootlog::note("toast suppressed (notifications disabled): stats rotation");
                }
                gui.send(GuiMsg::Alert);
            }

            // 分片睡眠，响应立即刷新 / 唤醒信号 / 退出。
            // D-65：睡眠预算由重连状态机裁决——正常态用轮询周期；无设备态
            // 按退避节奏提前醒来重枚举；有强制重枚举请求时立即醒。
            // WP-C（v3 C-1）：重连状态机的下次唤醒点仍以配置档为输入
            //（per-device 节奏只影响设备侧到期点，不改枚举/重连退避）。
            let (poll, device_due) = match core.lock() {
                Ok(c) => (
                    Duration::from_secs(c.config().poll_interval_secs),
                    c.next_poll_due(),
                ),
                Err(_) => (Duration::from_secs(60), None),
            };
            let interval = reconnect.next_wake(Instant::now(), !channels.is_empty(), poll);
            // WP-C（v3 C-1 修订）：睡眠预算 = min(全部在册真实设备的最近
            // 到期点, 重连状态机下次唤醒点)——旧 full_poll_due 的「下次
            // 必查时刻」角色由 AppCore::next_poll_due（per-device
            // next_due 最小值，含唤醒信号装填的「尽快」到期点）完全
            // 吸收，不再单独维护；无排程设备时退化为重连唤醒点。
            let deadline = match device_due {
                Some(d) => (Instant::now() + interval).min(d),
                None => Instant::now() + interval,
            };
            // v0.3.14：watcher 原本一轮 io round 才 poll 一次——默认轮询
            // 周期 1800s 意味着手改 TOML 最长 ~30 分钟才生效（用户体感
            // 「改配置不生效」的另一半根因）。mtime stat 是主机侧零射频
            // 操作，不触碰设备、不构成设备轮询，~5s 查一次零耗电代价。
            let mut last_cfg_check = Instant::now();
            while Instant::now() < deadline {
                // v0.3.15（P2-1①）：等待分片从固定 200ms 放宽到
                // min(距 deadline 余量, 5s)——空闲期 recv 唤醒次数降
                // 25 倍；命令依旧随时可打断（阻塞上限即分片长度），
                // ~5s 配置热查节奏与 deadline 语义均不变。
                let slice = deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_secs(5));
                if slice.is_zero() {
                    break;
                }
                match cmd.recv_timeout(slice) {
                    // D-65：「立即刷新」必须先重枚举再查询——重启/重插后
                    // 缓存句柄可能已失效，只查旧句柄等于没刷。
                    Ok(IoCmd::RefreshNow) => {
                        reconnect.request_refresh();
                        continue 'outer;
                    }
                    // 被动唤醒（D-33）：装填一次唤醒探测资格并立即跑一轮——
                    // 恢复延迟取决于用户动鼠标的那一刻，而不是轮询周期。
                    Ok(IoCmd::WakeSignal(raw_path)) => {
                        // E22：记录唤醒信号时刻，供重枚举唤醒恢复窗口判定。
                        last_wake_signal_at = Some(Instant::now());
                        // 融合稿 Phase 1：RawInput 已观测到罗技鼠标活动 →
                        // 对应本地分钟桶置位（系统事件推送消费，零设备 I/O）。
                        if let Some(path) = &raw_path {
                            let mark_ts = wall_secs() as i64;
                            // FR-1（RB2）：桶键命名空间统一——mark 前先按
                            // raw path 匹配当前通道/槽位活映射取 unitId
                            //（与 Read 事件同一命名空间），miss 再查
                            // last_known.json 持久映射，双 miss 才落原路径
                            // 键；双 miss 计入 channel_map_miss 同类遥测
                            //（计数分工见下方 mark 前的补计分支注释）。
                            let (bucket_key, key_src) = {
                                let candidates: Vec<(String, String)> = channels
                                    .iter()
                                    .flat_map(|ch| {
                                        let key = ch.key.clone();
                                        [&ch.info.short_path, &ch.info.long_path]
                                            .into_iter()
                                            .flatten()
                                            .map(move |p| {
                                                (key.clone(), p.to_string_lossy().into_owned())
                                            })
                                    })
                                    .collect();
                                let mut resolved = false;
                                let mut active_unit: Option<String> = None;
                                if let ChannelMatch::Hit(ci) =
                                    channel_for_raw_path_classified(path, &candidates)
                                {
                                    let chan_key = &candidates[ci].0;
                                    resolved = true;
                                    // 通道恰一个去重后非空 unitId 才可归因
                                    //（多槽别名 / 身份不明 → None，走持久
                                    // 映射兜底）。
                                    let mut units: HashSet<&str> = channels
                                        .iter()
                                        .filter(|ch| &ch.key == chan_key)
                                        .flat_map(|ch| {
                                            ch.slots.iter().filter_map(|s| s.unit_id.as_deref())
                                        })
                                        .collect();
                                    units.retain(|u| !u.is_empty());
                                    if units.len() == 1 {
                                        active_unit =
                                            units.into_iter().next().map(|u| u.to_string());
                                    }
                                }
                                let persistent = store.path_map_unit_id(path);
                                let (k, src) = activity_bucket_key(
                                    active_unit.as_deref(),
                                    persistent.as_deref(),
                                    path,
                                    resolved,
                                );
                                (k, src)
                            };
                            // 仅「归因命中通道但槽位无 unitId」由桶键侧补计
                            //（归因 Hit 分支不计 miss，只有桶键侧看得见）；
                            // 双 miss 场景由下方归因 miss 分支各自计数，
                            // 此处不重复计——保持 channel_map_miss 的
                            //「per 唤醒信号归因事件 +1」口径（diag_export ③）。
                            if matches!(key_src, ActivityKeySource::RawPathChannelKeyUnresolved) {
                                channel_map_miss.fetch_add(1, Ordering::Relaxed);
                            }
                            activity_buckets.mark(&bucket_key, mark_ts);
                            // 复核批 F3（融合稿 §5）：boundary 冲刷——本地
                            // 日界轮转后，把早于当日的已完成日位图并入 stats
                            // 事件流（当日未完成桶留内存待 run_stop）。首次
                            // mark 同样过一遍：本 run 从昨天跑到今天时，
                            // 昨天的桶在此冲出；无更早日期时取空、不产生
                            // 空摘要事件（note_activity_buckets 拒空）。
                            let today = activity_buckets.local_date(mark_ts);
                            if last_bucket_date.as_deref() != Some(today.as_str()) {
                                for (dev, days) in activity_buckets.take_completed_before(&today) {
                                    stats_store.note_activity_buckets(
                                        &dev,
                                        days,
                                        mark_ts,
                                        Instant::now(),
                                    );
                                }
                                last_bucket_date = Some(today);
                            }
                        }
                        // v0.3.11（F5）：0 注册槽位时（重启后鼠标休眠、槽位
                        // 空，未注册设备收不到唤醒探测资格）——唤醒信号 =
                        // Raw Input 已观测到罗技鼠标活动，设备已被用户物理
                        // 唤醒、处于活跃态：置 force 立即强制重枚举+扫槽
                        // （主机侧发起的补读，不违反零鼠标电池红线——红线
                        // 禁止主动唤醒休眠设备，不禁止设备已醒时补读）。
                        // 防抖在 Reconnect::request_wake_enum（事件风暴
                        // 合并，不形成枚举风暴）。
                        let registered_slots: usize = channels.iter().map(|c| c.slots.len()).sum();
                        if reconnect.request_wake_enum(Instant::now(), registered_slots) {
                            bootlog::note(
                                "wake signal with 0 registered slots: force re-enumerate \
                                 (device physically awake; host-side scan)",
                            );
                        }
                        // v0.3.15（P1-1）：唤醒信号只装填探测资格——有设备
                        // 被装填、或恰好到了枚举门控点，才跑一轮「仅探测」；
                        // 无事可做的唤醒信号不再触发整轮重查（裁决纯逻辑
                        // wake_action 在 app 层单测覆盖）。
                        // R-3（v0.3.17）：装填全集扩展为 Sleeping ∪
                        // (Online ∧ 灰显)——灰显在线设备电量未知，鼠标已
                        // 活动时装填一次探测资格提前刷新；装填集裁决纯逻辑
                        // wake_arm_set 在 app 层单测覆盖（四例矩阵）。
                        // WP-A（v3 A-3 通道归因）：Raw Input hDevice → 设备
                        // 接口路径 → HID 通道指纹唯一匹配，只装填该通道上的
                        // 设备；无路径 / 失配 / 多配 → 回退 arm-all 并记
                        // channel_map_miss（进诊断导出）。arm-all 仅装填
                        // Sleeping，但 Online-灰显恰为 1 台时一并装填
                        //（R-3 豁免：误装填他机代价为零）。BT/有线系统性
                        // 失配为 v3 明示的已知降级。
                        let (armed_any, attribution, n_sleeping, n_gray) = if let Ok(mut c) =
                            core.lock()
                        {
                            let sleeping = c.sleeping_ids();
                            let online_gray = c.online_gray_ids(Instant::now());
                            let n_sleeping = sleeping.len();
                            let n_gray = online_gray.len();
                            let (ids, attribution, exempt) = match raw_path.as_deref() {
                                Some(rp) => {
                                    let mut candidates: Vec<(String, String)> = Vec::new();
                                    for ch in &channels {
                                        for p in [&ch.info.short_path, &ch.info.long_path]
                                            .into_iter()
                                            .flatten()
                                        {
                                            candidates.push((
                                                ch.key.clone(),
                                                p.to_string_lossy().into_owned(),
                                            ));
                                        }
                                    }
                                    match channel_for_raw_path_classified(rp, &candidates) {
                                        ChannelMatch::Hit(ci) => {
                                            let key = candidates[ci].0.clone();
                                            let on_chan: HashSet<&str> = channels
                                                .iter()
                                                .filter(|ch| ch.key == key)
                                                .flat_map(|ch| {
                                                    ch.slots.iter().map(|s| s.id.as_str())
                                                })
                                                .collect();
                                            let (ids, _) = wake_arm_set(
                                                &sleeping,
                                                &online_gray,
                                                Some(&on_chan),
                                            );
                                            (ids, "hit", false)
                                        }
                                        ChannelMatch::Ambiguous => {
                                            channel_map_miss.fetch_add(1, Ordering::Relaxed);
                                            // R-4/R-11（v0.3.17）：miss 子
                                            // 分类计数 + 采样日志（每分
                                            // 钟 1 条+计数）。
                                            telemetry
                                                .wake_miss_ambiguous
                                                .fetch_add(1, Ordering::Relaxed);
                                            if let Some(line) = wake_miss_sampler.note_miss(
                                                    "wake channel attribution miss (ambiguous); fallback arm-all",
                                                    Instant::now(),
                                                ) {
                                                    bootlog::note(&line);
                                                }
                                            let (ids, exempt) =
                                                wake_arm_set(&sleeping, &online_gray, None);
                                            (ids, "miss-ambiguous", exempt)
                                        }
                                        ChannelMatch::ZeroMatch => {
                                            channel_map_miss.fetch_add(1, Ordering::Relaxed);
                                            // E11：零候选匹配（系统性失配：
                                            // BT / 有线 / 他机接收器）与指
                                            // 纹不可解析分桶，各自独立计数。
                                            telemetry
                                                .wake_miss_zero_match
                                                .fetch_add(1, Ordering::Relaxed);
                                            if let Some(line) = wake_miss_sampler.note_miss(
                                                    "wake channel attribution miss (zero-match); fallback arm-all",
                                                    Instant::now(),
                                                ) {
                                                    bootlog::note(&line);
                                                }
                                            let (ids, exempt) =
                                                wake_arm_set(&sleeping, &online_gray, None);
                                            (ids, "miss-zero-match", exempt)
                                        }
                                        ChannelMatch::NoFingerprint => {
                                            channel_map_miss.fetch_add(1, Ordering::Relaxed);
                                            // E11：指纹不可解析（缺失/畸形）
                                            // 独立计数。
                                            telemetry
                                                .wake_miss_no_fingerprint
                                                .fetch_add(1, Ordering::Relaxed);
                                            if let Some(line) = wake_miss_sampler.note_miss(
                                                    "wake channel attribution miss (no-fingerprint); fallback arm-all",
                                                    Instant::now(),
                                                ) {
                                                    bootlog::note(&line);
                                                }
                                            let (ids, exempt) =
                                                wake_arm_set(&sleeping, &online_gray, None);
                                            (ids, "miss-no-fingerprint", exempt)
                                        }
                                    }
                                }
                                None => {
                                    channel_map_miss.fetch_add(1, Ordering::Relaxed);
                                    telemetry.wake_miss_no_path.fetch_add(1, Ordering::Relaxed);
                                    if let Some(line) = wake_miss_sampler.note_miss(
                                            "wake channel attribution miss (no raw path); fallback arm-all",
                                            Instant::now(),
                                        ) {
                                            bootlog::note(&line);
                                        }
                                    let (ids, exempt) = wake_arm_set(&sleeping, &online_gray, None);
                                    (ids, "miss-no-path", exempt)
                                }
                            };
                            if exempt {
                                // R-3：arm-all 豁免——Online-灰显恰为
                                // 1 台，一并装填（误装填他机代价为零）。
                                bootlog::note(
                                    "wake arm-all exemption: exactly 1 online-gray device armed",
                                );
                            }
                            if attribution != "hit" {
                                // R-11：arm-all 装填数（诊断导出）。
                                telemetry
                                    .wake_arm_all_fills
                                    .fetch_add(ids.len() as u64, Ordering::Relaxed);
                            }
                            for id in &ids {
                                // WP-C：装填同时把该设备到期点提到「尽快」
                                //（冷却窗口内排在冷却结束点，I-12 不变）。
                                c.note_wake_signal(id, Instant::now());
                            }
                            (!ids.is_empty(), attribution, n_sleeping, n_gray)
                        } else {
                            (false, "core-lock-failed", 0, 0)
                        };
                        let enum_due =
                            reconnect.should_enumerate(Instant::now(), !channels.is_empty());
                        let action = wake_action(armed_any, enum_due);
                        // R-4（v0.3.17）：唤醒裁决链落 bootlog——归因
                        // 结果、装填构成、枚举门控与最终裁决（集合可能很
                        // 大，只记数量不记 id）。
                        // BF-2（v0.3.17）＋ E22（报告口径）：零信息稳态
                        //（attribution 为 hit 或 miss-* ∧ 未装填 ∧
                        // stay-asleep，鼠标活动稳态高频行）经采样器每
                        // 分钟 1 条+计数；状态跃迁（桶切换 / 装填 / 探测 /
                        // core-lock-failed）仍逐条保留，跃迁时冲出上一
                        // 桶被压制的计数（不丢数）。
                        let decision_line = format!(
                            "wake decision: attribution={attribution} armed={armed_any} (sleeping={n_sleeping} online-gray={n_gray}) enum_due={enum_due} action={}",
                            match action {
                                WakeAction::ProbeRound => "probe-round",
                                WakeAction::StayAsleep => "stay-asleep",
                            }
                        );
                        let steady = attribution != "core-lock-failed"
                            && !armed_any
                            && matches!(action, WakeAction::StayAsleep);
                        if steady {
                            for line in wake_steady_sampler.note_steady(
                                attribution,
                                &decision_line,
                                Instant::now(),
                            ) {
                                bootlog::note(&line);
                            }
                        } else {
                            if let Some(line) = wake_steady_sampler.leave_steady() {
                                bootlog::note(&line);
                            }
                            bootlog::note(&decision_line);
                        }
                        match action {
                            WakeAction::ProbeRound => {
                                probe_only_next = true;
                                continue 'outer;
                            }
                            WakeAction::StayAsleep => {}
                        }
                    }
                    // WP-B：统计导出——IO 线程后台执行（v3 P2-③ 不阻塞
                    // UI；只读本机 stats 文件 + 渲染写盘，零设备查询，
                    // 电池红线不违）。完成后经 GuiSink 通知 GUI 弹结果。
                    Ok(IoCmd::ExportStats(req)) => {
                        let msg = run_stats_export(&mut stats_store, &req);
                        gui.send(GuiMsg::StatsExportDone(msg));
                    }
                    Ok(IoCmd::Shutdown) => {
                        // 小修批第 3 项：会话收尾冲出两个唤醒日志采样器的
                        // pending 压制计数（同上，循环顶 Shutdown 同口径）。
                        if let Some(line) = wake_miss_sampler.flush_tail() {
                            bootlog::note(&line);
                        }
                        if let Some(line) = wake_steady_sampler.leave_steady() {
                            bootlog::note(&line);
                        }
                        // 融合稿 Phase 1：run_stop 并入活动桶摘要（同上，
                        // 循环顶 Shutdown 同口径）。
                        let bucket_ts = wall_secs() as i64;
                        for (dev, days) in activity_buckets.take_snapshot() {
                            stats_store.note_activity_buckets(
                                &dev,
                                days,
                                bucket_ts,
                                Instant::now(),
                            );
                        }
                        // WP-B：run_stop 生命周期事件 + 强制落盘。
                        stats_store.note_run_stop(wall_secs() as i64, Instant::now());
                        if let Err(e) =
                            stats_store.shutdown_flush(Instant::now(), wall_secs() as i64)
                        {
                            bootlog::note(&format!("stats.jsonl shutdown flush failed: {e}"));
                        }
                        if let Err(e) = store.shutdown_flush(Instant::now()) {
                            bootlog::note(&format!("last_known.json shutdown flush failed: {e}"));
                        }
                        // WP-B（I-02）：回执落盘完成。
                        let _ = shutdown_ack.send(());
                        return;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        if last_cfg_check.elapsed() >= Duration::from_secs(5) {
                            last_cfg_check = Instant::now();
                            handle_config_reload(
                                &mut watcher,
                                &core,
                                &observer,
                                &mut last_reload_warnings,
                            );
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
        }
    }

    /// 配置热重载处理（P1-2：所有权在 app 层）。从 io 循环体抽出，
    /// 供「每轮 io round」与「分片睡眠内 ~5s 节奏」两处调用，逻辑唯一。
    fn handle_config_reload(
        watcher: &mut ConfigWatcher,
        core: &Arc<Mutex<AppCore>>,
        observer: &Arc<Mutex<Observer>>,
        last_reload_warnings: &mut Vec<String>,
    ) {
        if let Some(Ok((cfg, warnings))) = watcher.poll() {
            if let Ok(mut c) = core.lock() {
                c.swap_config(cfg);
                // D-74（相位二第 5 项）：换入即批量重排到期点——档位/实验
                // 开关变更即时生效（虚拟/休眠设备恒不排程，D-33 不受影响）。
                // 设置窗口改动经 TOML 落盘同样走本路径，无需单独通知机制。
                c.retier_all(Instant::now());
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
                } else if ws != *last_reload_warnings {
                    Some(DiagKind::ConfigReloaded {
                        warnings: ws.clone(),
                    })
                } else {
                    None
                };
                *last_reload_warnings = ws;
                if let Some(kind) = kind {
                    o.push(DiagEvent::new(Layer::App, kind));
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
        /// 充电快档实验开关镜像（本方案）：配置侧开关翻转经签名第 9
        /// 分量触发菜单重建，文案随 FastPollState 三态变化。
        experimental_fast_poll: bool,
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
        /// 上一 tick 的闪烁状态（v0.3.14，B9：闪烁启停 bootlog 的沿检测）。
        prev_blinking: bool,
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
                experimental_fast_poll: cfg.experimental_per_device_polling,
                blink_enabled: cfg.blink_enabled,
                low_threshold: cfg.low_threshold,
                tiers_enabled: cfg.icon_font_tiers_enabled,
                tier_thresholds: cfg.icon_font_tier_thresholds.clone(),
                tier_colors: cfg.icon_font_tier_colors.clone(),
                menu_sig: String::new(),
                blink: crate::blink::BlinkState::new(),
                prev_blinking: false,
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
        // v0.3.22（质检复核 P0-1）：「立即重启」的子实例带环境标记——
        // 旧实例优雅退出期间互斥体仍被持有，子实例识别标记后轮询等待
        // 释放（≤RESTART_WAIT_TIMEOUT）再进主流程；超时给明确失败提示
        // 并以非零码退出，不再撞「已在运行」弹窗后新旧双退。
        let restart_child = std::env::var_os(crate::RESTART_CHILD_ENV).is_some();
        if restart_child {
            // 标记只作用于启动判定，读后即清，避免向更下游传递。
            std::env::remove_var(crate::RESTART_CHILD_ENV);
            bootlog::note("restart child: waiting for previous instance to release mutex");
        }
        let _mutex = if restart_child {
            match acquire_restart_child() {
                Ok(RestartAcquire::Acquired(h)) => {
                    bootlog::note("restart child: mutex acquired, taking over");
                    h
                }
                Ok(RestartAcquire::Timeout) => {
                    bootlog::note(
                        "restart child: timed out waiting for mutex release, abort restart",
                    );
                    bootlog::fatal_box(
                        "LogiVoltTray 重启失败",
                        "旧实例未在限定时间内退出，本次重启已中止。请手动重新启动 LogiVoltTray。",
                    );
                    return 1;
                }
                Err(e) => {
                    bootlog::note(&format!("single instance error: {e}"));
                    bootlog::fatal_box("LogiVoltTray 启动失败", &format!("单实例初始化失败：{e}"));
                    return 1;
                }
            }
        } else {
            match acquire() {
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
            }
        };

        // SAFETY(E9): 仅向 SetProcessDpiAwarenessContext 传常量 DPI_AWARENESS_CONTEXT 值，进程启动早期单线程调用。
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
        let (mut cfg, load_warnings) = match Config::load(&cfg_path) {
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
                // v0.3.17（一轮 P0-⑤）：默认配置回写改 tmp→rename 原子
                // 写，失败落 bootlog 不再静默。
                if let Err(e2) = write_tmp_rename(&cfg_path, &c.to_toml()) {
                    bootlog::note(&format!(
                        "default config persist to {} failed: {e2}",
                        cfg_path.display()
                    ));
                }
                (c, Vec::new())
            }
        };

        // v0.3.22（D-86；质检复核 P1-1 修订）：Q3 一次性提示——展示判定
        // 与闩锁结算拆成两段：配置加载后无条件结算一次闩锁并落盘（无论
        // 本轮是否实际弹提示，首个 v0.3.22 启动都应取走，避免首启
        // due()=false 的用户闩锁残留、日后「关掉一项再重启」迟到补弹）；
        // 是否弹 toast 仍由取走前的 default_on_notice_due() 判定决定。
        // 不新增 v0322 迁移标记（C-09：显式值永不被覆盖，存量 false 保持
        // false 由用户自行对齐）。
        let (notice_due, latch_taken) = cfg.settle_default_on_notice();
        if latch_taken {
            if let Err(e) = write_tmp_rename(&cfg_path, &cfg.to_toml()) {
                bootlog::note(&format!("default-on notice latch persist failed: {e}"));
            }
        }
        if notice_due {
            let n = cfg.experimental_enabled_count();
            let body = format!(
                "LogiVoltTray v0.3.22 起试验功能默认开启；你当前已开启 {n}/4 项，可到 设置→精细化设置 一键对齐"
            );
            if let Err(e) = toast::notify("实验室（试验功能）默认开启", &body, true) {
                bootlog::note(&format!("default-on notice toast failed: {e}"));
            }
        }
        let config_arc = Arc::new(RwLock::new(cfg.clone()));
        let observer = Arc::new(Mutex::new(Observer::new()));
        // v0.3.12（D-69）：最近一次枚举快照——IO 线程每轮枚举后写入，
        // GUI 线程「导出诊断」时读取（重启后识别失败场景下，导出不再
        // 只有空设备列表，还能带上枚举现场）。
        let enum_snap = Arc::new(Mutex::new(None::<EnumSnapshot>));
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

        // v0.3.19（U-7/C-1）：启动自愈对账——旧版勾选自启后换新版（exe
        // 文件名带版本号）时，注册表 Run 值仍指向旧路径、设置界面却显示
        // 「已开」。启动时按「配置 × 注册表」四格矩阵收口：配置开＋值缺失
        // →重建；配置开＋路径陈旧→重写为当前 exe（保留用户附加参数）；
        // 配置关＋值残留→清理；稳态→不动。必须在 GuiState 初始化前执行，
        // 使界面镜像自修复后状态。debug 构建跳过（C-2f 开发态防护）；
        // 成功静默仅 bootlog 一行（C-2e）；读失败推一次性 Alert；写失败
        // 沿用 cfg_changed 回路既有语义——共享配置回退为注册表现状 +
        // persist_bool 落盘 + Alert（防每个 Snapshot 重试刷 UI，BF-5）。
        bootlog::stage("autostart-reconcile");
        let reconcile_cfg_on = cfg.autostart;
        let reconcile_cfg_path = cfg_path.clone();
        if let Err(e) = volttray_app::guarded("autostart-reconcile", || match autostart::reconcile(
            reconcile_cfg_on,
            &exe,
        ) {
            autostart::ReconcileOutcome::NoAction | autostart::ReconcileOutcome::SkippedDev => {}
            autostart::ReconcileOutcome::Repaired { old, new } => {
                bootlog::note(&format!(
                    "autostart reconciled: {} -> {}",
                    old.as_deref().unwrap_or("<missing>"),
                    new.as_deref().unwrap_or("<removed>")
                ));
            }
            autostart::ReconcileOutcome::ReadFailed(reason) => {
                bootlog::note(&format!("autostart state unreadable: {reason}"));
                if let Ok(mut o) = observer.lock() {
                    o.push(DiagEvent::new(
                            Layer::App,
                            DiagKind::Alert {
                                level: "warning".into(),
                                message: format!(
                                    "自启注册表状态无法核实（{reason}）；设置中的自启开关可能与实际不符"
                                ),
                            },
                        ));
                }
            }
            autostart::ReconcileOutcome::WriteFailed {
                registry_on,
                reason,
            } => {
                bootlog::note(&format!("autostart reconcile write failed: {reason}"));
                if let Ok(mut guard) = config_arc.write() {
                    guard.autostart = registry_on;
                }
                persist_bool(&reconcile_cfg_path, "autostart", registry_on);
                if let Ok(mut o) = observer.lock() {
                    o.push(DiagEvent::new(
                        Layer::App,
                        DiagKind::Alert {
                            level: "warning".into(),
                            message: format!(
                                "autostart registry sync failed; config reverted to {registry_on}"
                            ),
                        },
                    ));
                }
            }
        }) {
            bootlog::note(&format!("autostart-reconcile: {e}"));
        }

        let (gui_tx, gui_rx) = mpsc::channel::<GuiMsg>();
        let (cmd_tx, cmd_rx) = mpsc::channel::<IoCmd>();
        // WP-A：通道归因失配计数（诊断导出可见，v3 红队 P2-6）。
        let channel_map_miss = Arc::new(AtomicU64::new(0));
        // E8（红蓝对抗终审）：帧级（FRAME_STATS）与唤醒路径（WAKE_*）
        // 遥测原为进程级静态量，现收编进 Telemetry 由 run 持有，经 Arc
        // 穿线到 io_loop（SharedChannel 挂钩）与 pump（诊断导出读取）。
        let telemetry = Arc::new(Telemetry::new());
        // WP-B（I-02）：Shutdown 落盘回执通道——io_loop 在 run_stop +
        // 双 store flush 完成后回执；ack 接收端交给 WM_ENDSESSION 承接
        // 窗口（GUI 菜单退出经同一 hook 等待）。
        let (ack_tx, ack_rx) = mpsc::channel::<()>();
        {
            let core = core.clone();
            let observer = observer.clone();
            let path = cfg_path.clone();
            let enum_snap = enum_snap.clone();
            // v0.3.15（P2-1②配套）：闪烁定时器改事件驱动后，消息泵不再
            // 有 500ms 心跳——IO 线程的快照/告警需经 GuiSink 附带一条
            // 线程消息（WM_APP_GUI）唤醒 GetMessageW，否则空闲期 UI
            // 更新被无限推迟。tid 取的是本（GUI）线程 id。
            let gui_sink = GuiSink {
                tx: gui_tx,
                // SAFETY(E9): GetCurrentThreadId 无参、返回本（GUI）线程 id 值，无指针契约。
                tid: unsafe { GetCurrentThreadId() },
            };
            let channel_map_miss_io = channel_map_miss.clone();
            let telemetry_io = telemetry.clone();
            std::thread::spawn(move || {
                io_loop(
                    core,
                    observer,
                    gui_sink,
                    cmd_rx,
                    path,
                    enum_snap,
                    channel_map_miss_io,
                    telemetry_io,
                    ack_tx,
                )
            });
        }
        bootlog::stage("io-thread-spawned");

        // WP-B（I-02，R5 终裁 P1）：常驻隐藏顶层窗口承接 WM_QUERYENDSESSION
        // / WM_ENDSESSION——Raw Input 窗口为 HWND_MESSAGE message-only，
        // 按 Win32 语义不接收广播消息，关机 flush 此前静默失效。窗口须在
        // GUI 线程创建（本线程消息泵负责分发其窗口消息）。安装失败仅降级
        //（关机路径退回与旧版一致），不阻断启动。
        crate::statsexport::install_endsession_window(cmd_tx.clone(), ack_rx);

        // 4. 图标缓存（v0.3.15 P1-2：懒渲染 + 小容量 LRU，不再启动全量
        //    预渲染；v0.3.0：形状/背景/字色/字体 + 低电提亮档，M9/M11）+ 托盘。
        // SAFETY(E9): GetDpiForSystem 无参、返回 u32 值，无指针契约。
        let dpi = unsafe { GetDpiForSystem() };
        let dark = system_dark_theme();
        let mut cache = cache_from_config(&cfg, dpi_bucket_size(dpi), dark);
        let mut cur_size = dpi_bucket_size(dpi);
        let mut cur_dark = dark;

        let mut state = GuiState::from_config(&cfg);

        bootlog::stage("icon-cache (lazy+LRU, no pre-render)");
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
                                "托盘图标首帧渲染失败（出厂默认渲染配置也无法生成默认帧，\
                                 疑似系统级 GDI 故障）：{e2}。程序即将退出。"
                            ),
                        );
                        return 1;
                    }
                }
            }
        };
        // 充电快档三态（本方案）：启动点设备列表为空 → fast_poll_state
        // 恒 Off（无在排程设备），与首帧菜单一起在首个 Snapshot 到达时经
        // 签名分量重建为实际态。
        let fast_state = core
            .lock()
            .map(|c| c.fast_poll_state(state.experimental_fast_poll))
            .unwrap_or(FastPollState::Off);
        let (menu_obj, mut ids) = build_menu(
            &[],
            None,
            None,
            state.poll_secs,
            state.blink_enabled,
            state.autostart_on,
            state.silent,
            fast_state,
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

        // v0.3.14（真机反馈 #3「两种频率叠加闪烁」根因修复）：hwnd=NULL 的
        // SetTimer 由系统分配 timer id（传入的 nIDEvent 被忽略，返回值才是
        // 真实 id），WM_TIMER 进入线程队列时 wParam 携带该 id。消息泵只响应
        // 本定时器——线程上其它窗口的定时器消息（设置窗口 tooltip 控件的
        // reshow 定时器约 100ms 级）若也驱动闪烁相位翻转，就会在正常 1Hz
        // 闪烁上叠加出「极快闪烁」。
        // windows crate 0.62：SetTimer 直接返回 usize 系统分配 id，0 = 失败
        // （GetLastError 语义）；失败时退化为不闪烁（bootlog 留痕，不 panic）。
        // v0.3.15（P2-1②）：定时器不再 24/7 常开——0 = 未 arm；仅在闪烁
        // 激活期间由 sync_blink_timer 惰性 SetTimer，停闪即 KillTimer。
        // id 过滤口径不变（真实 id + NULL hwnd 双重匹配）。
        let mut blink_timer = 0usize;

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
        let mut last_wake_sent: Option<Instant> = None;

        // 5. 消息泵（v0.3.15 P2-1②：事件驱动——不再 500ms 常开心跳；
        //    闪烁相位仍只由本程序 500ms 定时器的真实 tick 翻转）。
        bootlog::stage("message-pump");
        let mut msg = MSG::default();
        // SAFETY(E9): msg 为本线程栈上 MSG；GetMessageW 等按 Win32 消息循环契约使用，定时器与窗口句柄均属本线程。
        unsafe {
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                // 只响应本程序的 500ms 闪烁定时器（按 timer id + 线程定时器
                // 的 NULL hwnd 双重过滤；v0.3.14 #3：外来 WM_TIMER——如设置
                // 窗口 tooltip 的 reshow 定时器——不得驱动闪烁相位）。
                let is_blink_tick = msg.message == WM_TIMER
                    && blink_timer != 0
                    && msg.wParam.0 == blink_timer
                    && msg.hwnd == HWND::default();
                if msg.message == WM_INPUT {
                    // D-33 被动唤醒：系统层观测到罗技鼠标活动（移动/点击
                    // 本就产生射频流量，属用户行为零额外成本）→ 通知 IO 线程
                    // 装填一次唤醒探测资格。2s 合并防 WM_INPUT 风暴。
                    //
                    // 启动期 panic 修复（v0.3.8 真机 panic 同类隐患）：
                    // 原写法 `Instant::now() - Duration::from_secs(10)`
                    // 意图是「视为 10 秒前已发送过」——但 Windows 的 Instant
                    // 基于 QPC、基数自系统启动起算，开机自启时 uptime 可能
                    // 不足 10s，`Instant - Duration` 直接 underflow panic
                    // （std time.rs「overflow when subtracting duration from
                    // instant」）。改用 Option：None = 从未发送过，首个
                    // Raw Input 立即放行——与原语义等价（10s > 2s 合并窗，
                    // 启动后首包本就不受 2s 限制），且不引入任何额外轮询。
                    if rawinput::is_logitech_mouse_input(msg.lParam)
                        && last_wake_sent.is_none_or(|t| t.elapsed() >= Duration::from_secs(2))
                    {
                        last_wake_sent = Some(Instant::now());
                        let _ = cmd_tx.send(IoCmd::WakeSignal(rawinput::device_path(msg.lParam)));
                    }
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
                // v0.3.15（P2-1②）：pump 由事件驱动——真实闪烁 tick、
                // GuiSink 唤醒（WM_APP_GUI）、托盘/菜单事件各跑一轮；
                // WM_INPUT 风暴期不跑（IO 侧 2s 合并已限流，其产出的
                // 快照会经 GuiSink 线程消息另行唤醒 pump）。
                if is_blink_tick || msg.message != WM_INPUT {
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
                        &enum_snap,
                        &channel_map_miss,
                        &telemetry,
                        &config_arc,
                        &cfg_path,
                        &exe,
                        &mut ids,
                        is_blink_tick,
                        &mut blink_timer,
                    );
                }
            }
            if blink_timer != 0 {
                let _ = KillTimer(None, blink_timer);
            }
        }
        bootlog::stage("message-pump-exit");
        0
    }

    /// v0.3.15（P2-1②）：闪烁定时器随激活态惰性 arm / 停闪即 kill。
    /// arm 失败（返回 0）退化为不闪烁并留痕——与 v0.3.14 的降级口径一致。
    fn sync_blink_timer(blink_timer: &mut usize, want: bool) {
        // SAFETY(E9): SetTimer/KillTimer 以 hwnd=None 作用于本 GUI 线程消息队列；timer id 由 blink_timer 独占管理，arm 失败退化为不闪烁。
        unsafe {
            if want && *blink_timer == 0 {
                *blink_timer = SetTimer(None, 0, 500, None);
                if *blink_timer == 0 {
                    bootlog::note("blink SetTimer failed (returned 0); blink degraded off");
                }
            } else if !want && *blink_timer != 0 {
                let _ = KillTimer(None, *blink_timer);
                *blink_timer = 0;
            }
        }
    }

    /// v0.3.22（质检复核 P0-1）：构造「立即重启」的子实例启动命令——
    /// 注入 RESTART_CHILD_ENV 环境标记，子实例据此等待旧实例释放互斥体。
    /// 抽出为独立函数以便回归断言直接验证标记注入（Command::get_envs）。
    pub(crate) fn restart_command(exe: &Path) -> std::process::Command {
        let mut cmd = std::process::Command::new(exe);
        cmd.env(crate::RESTART_CHILD_ENV, "1");
        cmd
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
        enum_snap: &Arc<Mutex<Option<EnumSnapshot>>>,
        channel_map_miss: &Arc<AtomicU64>,
        telemetry: &Telemetry,
        cfg_arc: &Arc<RwLock<Config>>,
        cfg_path: &Path,
        exe: &Path,
        ids: &mut crate::menu::MenuIds,
        phase_tick: bool,
        blink_timer: &mut usize,
    ) {
        // v0.3.22（D-88；质检复核 P0-1 修订）：「立即重启」——复用既有
        // 启动路径 spawn 当前 exe 并注入 RESTART_CHILD_ENV 环境标记，子
        // 实例识别标记后轮询等待本进程退出释放互斥体（≤5s）再进主流程，
        // 消除「先 spawn 再优雅退出」与 acquire() 不等待互斥体之间的竞态；
        // 随后走既有退出序列（IoCmd::Shutdown + ENDSESSION_HOOK ack ≤2s +
        // PostQuitMessage）；排程代码零改动、非热生效。
        if crate::RESTART_REQUESTED.swap(false, Ordering::SeqCst) {
            bootlog::note("restart requested: spawn new instance then graceful shutdown");
            match restart_command(exe).spawn() {
                Ok(_) => {}
                Err(e) => bootlog::note(&format!("restart spawn failed: {e}")),
            }
            let _ = cmd_tx.send(IoCmd::Shutdown);
            if let Ok(guard) = crate::statsexport::ENDSESSION_HOOK.lock() {
                if let Some((_, rx)) = guard.as_ref() {
                    let _ = rx.recv_timeout(Duration::from_secs(2));
                }
            }
            // SAFETY(E9): PostQuitMessage 仅向本线程消息队列投递退出码，无指针参数。
            unsafe {
                PostQuitMessage(0);
            }
            return;
        }
        // 应用层消息。
        let mut snap_changed = false;
        while let Ok(m) = gui_rx.try_recv() {
            match m {
                GuiMsg::Snapshot(s) => {
                    // T6：rebind 改变条目 id 后，用户 pin 选定经物理身份
                    // （unitId）重匹配到新 id——拔线/插线切换端点后 pin
                    // 不丢；身份不明或设备真消失时保留原 pin 走既有回退。
                    if let Some(old) = state.snapshot.as_ref() {
                        if let Some(new_id) = repin(&state.pinned, old, &s) {
                            state.pinned = Some(new_id);
                        }
                    }
                    state.snapshot = Some(*s);
                    snap_changed = true;
                }
                GuiMsg::Alert => {} // toast 已在 IO 线程发出；此处仅占位便于扩展。
                GuiMsg::StatsExportDone(text) => {
                    // WP-B：导出结果提示（IO 线程已完成写盘；模态结果框，
                    // 成功列文件 / 失败给原因 / 部分成功如实分列）。
                    let title = wide("LogiVoltTray 耗电统计导出");
                    let body = wide(&text);
                    // SAFETY(E9): body/title 为 NUL 结尾宽字符缓冲，MessageBoxW 同步调用期间存活；其余参数为值类型。
                    unsafe {
                        MessageBoxW(
                            None,
                            PCWSTR(body.as_ptr()),
                            PCWSTR(title.as_ptr()),
                            MB_OK | MB_ICONINFORMATION,
                        );
                    }
                }
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
                    || cfg.experimental_per_device_polling != state.experimental_fast_poll
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
                    state.experimental_fast_poll = cfg.experimental_per_device_polling;
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
        // SAFETY(E9): GetDpiForSystem 无参、返回 u32 值，无指针契约。
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
        // v0.3.14（蓝队 B3 必修）：设备灰显/休眠时**不改写**低电态——休眠
        // 并非低电解除，若按 gray → set_low_active(false) 会无条件清掉用户
        // 的单击停闪锁存（ack），唤醒后违背「停闪保持到本低电期结束」重新
        // 开闪。灰显期间保持既有 low_active，唤醒恢复后由 tracker 重新裁决。
        let low_active = match shown_device(state) {
            Some(d) if !d.gray => core
                .lock()
                .map(|mut c| c.is_low_battery(&d.id))
                .unwrap_or(false),
            _ => state.blink.low_active(),
        };
        // v0.3.14（红蓝共识 B1 必修）：阈值热改即时重估产生的告警走与
        // 轮询路径相同的 toast 派发（此前被丢弃，本次低电期 toast 永久
        // 丢失）。去重由 tracker 状态保证（每个低电期至多一条）。
        let pending = core
            .lock()
            .map(|mut c| c.take_pending_alerts())
            .unwrap_or_default();
        if !pending.is_empty() {
            let (notify_enabled, silent) = cfg_arc
                .read()
                .map(|c| (c.notifications_enabled, c.notification_silent))
                .unwrap_or((true, false));
            for (id, alert) in pending {
                let name = state
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.devices.iter().find(|d| d.id == id))
                    .map(|d| d.name.clone())
                    .unwrap_or_else(|| "设备".into());
                let (title, body) = alert_text(alert, &name);
                if notify_enabled {
                    match toast::notify(&title, &body, silent) {
                        Ok(()) => bootlog::note(&format!(
                            "toast dispatched (threshold hot-change): {title}"
                        )),
                        Err(e) => {
                            bootlog::note(&format!("toast dispatch failed title={title}: {e}"))
                        }
                    }
                } else {
                    bootlog::note(&format!(
                        "toast suppressed (notifications disabled): {title}"
                    ));
                }
            }
        }
        if state.blink.set_low_active(low_active) {
            snap_changed = true;
        }
        // v0.3.15（P2-1②）：相位翻转只在真实闪烁定时器 tick 上发生；
        // 事件驱动的其余 pump（快照/菜单/托盘事件唤醒）只同步启停
        // 状态、不翻相位——否则闪烁频率会被消息流量牵着走（v0.3.14
        // #3「两种频率叠加」的同类病，换成事件源同样成立）。
        let outcome = if phase_tick {
            state.blink.tick(state.blink_enabled)
        } else {
            state.blink.status(state.blink_enabled)
        };
        // v0.3.14（红蓝共识 B9 必修）：闪烁启停落 bootlog（此前零日志，
        // 真机反馈无从定位「闪没闪 / 为什么停」）。
        if outcome.blinking != state.prev_blinking {
            if outcome.blinking {
                bootlog::note(&format!(
                    "blink start (threshold={}, switch on)",
                    state.low_threshold
                ));
            } else {
                let reason = if !state.blink_enabled {
                    "switch off"
                } else if state.blink.acked() {
                    "user click (ack)"
                } else {
                    "low released"
                };
                bootlog::note(&format!("blink stop: {reason}"));
            }
            state.prev_blinking = outcome.blinking;
        }

        // v0.3.15（P2-1②）：提亮相位的周期性重绘只跟真实闪烁 tick 走；
        // 非 tick 的 pump 只在内容变化（快照/配置/DPI）时重绘——事件
        // 驱动后不再存在「每 500ms 无条件刷新」的常驻开销。
        if snap_changed || (phase_tick && outcome.blinking) {
            refresh_ui(tray, state, cache, ids, enum_snap, core);
        }

        // 托盘图标事件：仅「左键按下」停闪（M11「单击停闪」原意）。
        // v0.3.14（真机反馈 #2 根因修复）：此前任意 Click 事件（含右键）
        // 都停闪——右键恰恰是打开菜单的按键，用户「低电中右键开菜单 →
        // 重新勾选低电闪烁」时，开菜单这一下已把本次低电期的闪烁永久
        // 压住（ack 锁存到低电解除才复位），表现为「开了也不闪」。
        while let Ok(e) = TrayIconEvent::receiver().try_recv() {
            let stop_blink_click = matches!(
                e,
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Down,
                    ..
                }
            );
            if stop_blink_click && state.blink.acknowledge_click() {
                refresh_ui(tray, state, cache, ids, enum_snap, core);
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
                // v0.3.18（U-5）：预览走单一渲染源，需与托盘同一 DPI 档位
                // 位图边长（cur_size 随 DPI/配置变化在事件循环中维护）。
                crate::settings::open_settings(cfg_arc, cfg_path, *cur_dark, *cur_size, &preview);
            } else if e.id == ids.diag {
                export_diag(observer, enum_snap, channel_map_miss, telemetry);
            } else if e.id == ids.stats_export {
                // WP-B：导出耗电统计（选项对话框 → Save-As → IO 线程执行）。
                crate::statsexport::begin_export(cmd_tx);
            } else if e.id == ids.check_update {
                // T-07（实施令零网络形态）：手动「检查更新」＝ShellExecuteW
                // 交系统默认浏览器打开 /releases/latest——无 HTTP 客户端、
                // 零常驻外联、零启动自动检查（未验证项④：真机点击实际打开
                // 浏览器待 Windows 实测；此处仅常量 URL，零网络代码路径）。
                let url = wide("https://github.com/isxulong/Logi-VoltTray/releases/latest");
                let verb = wide("open");
                // SAFETY(E9): url/verb 为 NUL 结尾宽字符常量缓冲，ShellExecuteW 同步调用期间存活；仅打开常量 URL，无注入面。
                let _ = unsafe {
                    ShellExecuteW(
                        None,
                        PCWSTR(verb.as_ptr()),
                        PCWSTR(url.as_ptr()),
                        PCWSTR::null(),
                        PCWSTR::null(),
                        SW_SHOWNORMAL,
                    )
                };
            } else if e.id == ids.blink {
                state.blink_enabled = !state.blink_enabled;
                if let Ok(mut guard) = cfg_arc.write() {
                    guard.blink_enabled = state.blink_enabled;
                }
                persist_bool(cfg_path, "blink_enabled", state.blink_enabled);
                // v0.3.14（B9）：菜单开关切换落日志。
                bootlog::note(&format!(
                    "menu toggle: blink_enabled -> {}",
                    state.blink_enabled
                ));
                refresh_ui(tray, state, cache, ids, enum_snap, core);
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
                bootlog::note(&format!("menu toggle: autostart -> {}", state.autostart_on));
                refresh_ui(tray, state, cache, ids, enum_snap, core);
            } else if e.id == ids.silent {
                state.silent = !state.silent;
                if let Ok(mut guard) = cfg_arc.write() {
                    guard.notification_silent = state.silent;
                }
                persist_bool(cfg_path, "notification_silent", state.silent);
                bootlog::note(&format!(
                    "menu toggle: notification_silent -> {}",
                    state.silent
                ));
                refresh_ui(tray, state, cache, ids, enum_snap, core);
            } else if e.id == ids.exit {
                let _ = cmd_tx.send(IoCmd::Shutdown);
                // WP-B（I-02 同口径）：等 IO 线程 run_stop + 双 store
                // flush 落盘完成（≤2s）再退——此前 PostQuitMessage 后主
                // 进程立即退出，IO 线程的落盘存在被截断的竞态窗口。
                // 与 WM_ENDSESSION 承接窗口共用同一 ack 通道。
                if let Ok(guard) = crate::statsexport::ENDSESSION_HOOK.lock() {
                    if let Some((_, rx)) = guard.as_ref() {
                        let _ = rx.recv_timeout(Duration::from_secs(2));
                    }
                }
                // SAFETY(E9): PostQuitMessage 仅向本线程消息队列投递退出码，无指针参数。
                unsafe { PostQuitMessage(0) };
            } else if let Some((_, dev_id)) = ids.device_rows.iter().find(|(mid, _)| *mid == e.id) {
                state.pinned = Some(dev_id.clone());
                refresh_ui(tray, state, cache, ids, enum_snap, core);
            }
        }

        // v0.3.15（P2-1②）：每次 pump 末尾按闪烁激活态对齐定时器——
        // 激活期惰性 arm，停闪（含 ack / 低电解除 / 开关关闭）即 kill。
        sync_blink_timer(blink_timer, state.blink.is_blinking(state.blink_enabled));
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
        cache: &mut IconCache,
        ids: &mut crate::menu::MenuIds,
        enum_snap: &Arc<Mutex<Option<EnumSnapshot>>>,
        core: &Arc<Mutex<AppCore>>,
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
        // R1-F2（T-04③ 修复）：签名并入「枚举失败摘要」布尔——失败候选
        // 出现/消失本身即触发重建，不再依赖其余分量变化。
        let enum_failure_hint = {
            let guard = enum_snap.lock().ok();
            crate::menu::enum_failure_hint_applies(
                &devices,
                guard.as_ref().and_then(|g| g.as_ref()),
            )
        };
        let sig = crate::menu_content_sig(
            state.pinned.as_deref(),
            state.poll_secs,
            state.blink_enabled,
            state.autostart_on,
            state.silent,
            default_id.as_deref(),
            &devices,
            enum_failure_hint,
            state.experimental_fast_poll,
        );
        if sig != state.menu_sig {
            // 充电快档三态（本方案）：持 core 锁按当前设备快照与实验开关
            // 镜像计算（对账环已有 core.lock() 先例）；锁获取失败退 Off，
            // 不因菜单重建阻塞 GUI 线程，下个 tick 对账回写后再建。
            let fast_state = core
                .lock()
                .map(|c| c.fast_poll_state(state.experimental_fast_poll))
                .unwrap_or(FastPollState::Off);
            let (menu_obj, new_ids) = build_menu(
                &devices,
                default_id.as_deref(),
                state.pinned.as_deref(),
                state.poll_secs,
                state.blink_enabled,
                state.autostart_on,
                state.silent,
                fast_state,
            );
            // T-04③：枚举含蓝牙/有线候选且失败、而菜单无 direct 行时，追加禁用态提示行
            if let Ok(guard) = enum_snap.lock() {
                if let Some(es) = guard.as_ref() {
                    crate::menu::append_direct_enum_failure_hint(&menu_obj, &devices, Some(es));
                }
            }
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
        // D-33/E22：休眠/过期 = 灰显但保留最后已知电量数字。icon_text 内部
        // 已按 percent 有无区分（有读数渲染数字、无读数才 "--"），gray 只
        // 作用于颜色档与 tooltip 语义，不再把文本打成 "--"。
        let text = icon_text(percent, gray);
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
            Some(d) if d.gray && d.percent.is_some() => {
                format!(
                    "LogiVoltTray — {} {}（最后已知电量 · 显示已过期）",
                    d.name, text
                )
            }
            Some(d) => format!("LogiVoltTray — {} {}", d.name, text),
            None => "LogiVoltTray".to_string(),
        };
        let _ = tray.set_tooltip(Some(&tip));
    }

    #[cfg(test)]
    mod e10_regression_tests {
        use super::*;
        use volttray_app::reconnect::DEAD_SLOT_COOLDOWN_ROUNDS;

        /// E10 回归①（#255 负缓存）：缓存命中时——0x01 幽灵口径
        /// （cache_applies=true，对应 Hidpp1Ghost/Hidpp2）由缓存抑制探测；
        /// 可信 HID++1（cache_applies=false）放行探测由调用方清缓存
        /// （AF-6）；缓存未命中或非 0xFF 槽不抑制。
        #[test]
        fn ff_negative_cache_gate_truth_table() {
            // 幽灵/2.0 + 缓存命中 → 抑制（修复点：0x01 路径进入负缓存）。
            assert!(ff_probe_suppressed(0xFF, true, true));
            // 可信 HID++1 + 缓存命中 → 不抑制（AF-6 清缓存照常探测）。
            assert!(!ff_probe_suppressed(0xFF, true, false));
            // 缓存未命中 → 不抑制（首轮探测照常，失败后才置缓存）。
            assert!(!ff_probe_suppressed(0xFF, false, true));
            assert!(!ff_probe_suppressed(0xFF, false, false));
            // 非 0xFF 槽不适用负缓存。
            assert!(!ff_probe_suppressed(0x02, true, true));
        }

        /// E10 回归②（死槽冷却喂入）：接收器 0x08「槽位无设备」证据经
        /// `feeds_dead_slot_cooldown` 真实写入 DeadSlotScan——连续 K 轮后
        /// 进入冷却，冷却期内不重复判定（每 K 轮放行 1 次复扫）；Io 证据
        /// 不喂冷却（留给 SlotScanStreak 裁决句柄死亡）。
        #[test]
        fn dead_slot_cooldown_fed_by_no_device_evidence() {
            let mut d = DeadSlotScan::new();
            // 生产接线同形：try_slot 的 NoResponse 臂
            // `edge.handshake_no_response = cause.feeds_dead_slot_cooldown()`。
            let feed = NoResponseCause::NoDevice.feeds_dead_slot_cooldown();
            assert!(feed, "NoDevice 必须喂入死槽冷却");
            for _ in 0..DEAD_SLOT_COOLDOWN_ROUNDS {
                assert!(d.should_scan(2), "冷却前每轮都扫");
                d.note_handshake(2, feed);
            }
            assert!(d.should_scan(2), "streak 达 K 的整数倍放行最后 1 次");
            d.note_handshake(2, feed);
            for _ in 1..DEAD_SLOT_COOLDOWN_ROUNDS {
                assert!(!d.should_scan(2), "冷却期内不重复判定");
                d.note_handshake(2, feed);
            }
            assert!(d.should_scan(2), "每 K 轮放行 1 次复扫");
            // Io 不喂冷却：一次 Io 证据即复位（句柄可疑≠槽位死）。
            d.note_handshake(2, NoResponseCause::Io.feeds_dead_slot_cooldown());
            assert!(d.should_scan(2), "Io 证据复位冷却");
        }
    }

    #[cfg(test)]
    mod wake_path_tests {
        use super::*;

        /// R-4：miss 采样——60s 窗口内只发第 1 条，压制计数合并进下一条。
        #[test]
        fn miss_log_sampler_throttles_per_minute() {
            let mut s = MissLogSampler::new();
            let t0 = Instant::now();
            assert_eq!(s.note_miss("m", t0).as_deref(), Some("m"));
            assert_eq!(s.note_miss("m", t0 + Duration::from_secs(10)), None);
            assert_eq!(s.note_miss("m", t0 + Duration::from_secs(20)), None);
            assert_eq!(
                s.note_miss("m", t0 + Duration::from_secs(61)).as_deref(),
                Some("m (suppressed 2 similar in last 60s)")
            );
            assert_eq!(s.note_miss("m", t0 + Duration::from_secs(62)), None);
        }

        /// 小修批第 3 项：会话收尾冲出尾部 pending 压制计数，使 diag
        /// 计数器与日志可对平（计数器值 = 可见 miss 行 + Σ suppressed）。
        #[test]
        fn miss_log_sampler_flush_tail_drains_pending_suppressed() {
            let mut s = MissLogSampler::new();
            let t0 = Instant::now();
            // 无压制时尾部为空。
            assert_eq!(s.flush_tail(), None);
            // 窗口内压制 2 条后，尾部冲出并清零。
            assert_eq!(s.note_miss("m", t0).as_deref(), Some("m"));
            assert_eq!(s.note_miss("m", t0 + Duration::from_secs(10)), None);
            assert_eq!(s.note_miss("m", t0 + Duration::from_secs(20)), None);
            assert_eq!(
                s.flush_tail().as_deref(),
                Some(
                    "wake attribution miss sampler tail: suppressed 2 similar flushed at session end"
                )
            );
            // 冲出后计数已清零，重复调用不再产出。
            assert_eq!(s.flush_tail(), None);
            // 尾部冲出后新窗口首条不再叠加旧压制数。
            assert_eq!(
                s.note_miss("m", t0 + Duration::from_secs(61)).as_deref(),
                Some("m")
            );
        }

        /// R-11：帧遥测计数 / feature_index 分类 / 平均写间隔。E8：原全局
        /// 静态 FRAME_STATS 已收编进 Telemetry（由 run 持有）；本用例改用
        /// 独立实例，相对增量 / 快照重算断言（BF-6）口径原样保留。
        #[test]
        fn frame_stats_counts_and_averages() {
            let frame_stats = FrameStats::new();
            let tx0 = frame_stats.tx.load(Ordering::Relaxed);
            let rx0 = frame_stats.rx.load(Ordering::Relaxed);
            let (f_tx0, f_rx0, sum0, cnt0) = {
                let g = frame_stats.inner.lock().unwrap();
                (
                    g.feature_tx.get("0x10").copied().unwrap_or(0),
                    g.feature_rx.get("0x10").copied().unwrap_or(0),
                    g.tx_interval_sum_ms,
                    g.tx_interval_count,
                )
            };
            let t0 = Instant::now();
            frame_stats.note_tx(&[0x10, 0x00, 0x10], t0);
            frame_stats.note_tx(&[0x10, 0x00, 0x10], t0 + Duration::from_millis(500));
            frame_stats.note_rx(&[0x11, 0x00, 0x10]);
            assert_eq!(frame_stats.tx.load(Ordering::Relaxed), tx0 + 2);
            assert_eq!(frame_stats.rx.load(Ordering::Relaxed), rx0 + 1);
            {
                let g = frame_stats.inner.lock().unwrap();
                assert_eq!(g.feature_tx.get("0x10"), Some(&(f_tx0 + 2)));
                assert_eq!(g.feature_rx.get("0x10"), Some(&(f_rx0 + 1)));
                // 平均间隔按快照重算：本用例新增 1 个 500ms 样本。
                assert_eq!(g.tx_interval_sum_ms, sum0 + 500);
                assert_eq!(g.tx_interval_count, cnt0 + 1);
            }
            assert_eq!(
                frame_stats.avg_tx_interval_ms(),
                Some((sum0 + 500) / (cnt0 + 1))
            );
        }

        /// 一轮 P0-③：循环顶 RefreshNow 转发的时序语义——record_result
        /// 关门（force 已消费、30s 门控生效）后 request_refresh 重新
        /// 开门，should_enumerate 立即放行。修复前该命令在循环顶被
        /// 静默丢弃，本断言即竞态修复的锁定。
        #[test]
        fn refresh_now_at_loop_top_sets_pending_force() {
            let now = Instant::now();
            let mut r = Reconnect::new();
            r.record_result(now, 1);
            assert!(!r.should_enumerate(now, true));
            r.request_refresh();
            assert!(r.should_enumerate(now, true));
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

/// v0.3.22（D-88）：「立即重启」回归断言——确认重启走「置位标志 →
/// 主循环统一消费」的单次语义（防热生效回潮：设置窗口调用栈内不做任何
/// 重启动作），且标志消费后复位（二次 pump 不再触发）。
#[cfg(all(test, windows))]
mod restart_request_tests {
    use super::RESTART_REQUESTED;
    use std::sync::atomic::Ordering;

    #[test]
    fn restart_request_sets_then_consumes_once() {
        // 前置：确保从干净状态开始（单测试函数内串行操作同一全局静态，
        // 避免并行竞态）。
        RESTART_REQUESTED.store(false, Ordering::SeqCst);
        assert!(!RESTART_REQUESTED.load(Ordering::SeqCst));

        // 请求方（设置窗口 IDC_LAB_RESTART）仅置位——不在调用栈内重启。
        super::request_restart();
        assert!(RESTART_REQUESTED.load(Ordering::SeqCst));

        // 主循环消费（swap 取走并复位）：首次取走为 true（触发一次重启）。
        assert!(RESTART_REQUESTED.swap(false, Ordering::SeqCst));
        // 二次 pump：已复位，不再触发（一次性语义）。
        assert!(!RESTART_REQUESTED.swap(false, Ordering::SeqCst));
        assert!(!RESTART_REQUESTED.load(Ordering::SeqCst));

        // 重复点击不产生额外计数（布尔标志天然去重）。
        super::request_restart();
        super::request_restart();
        assert!(RESTART_REQUESTED.swap(false, Ordering::SeqCst));
        assert!(!RESTART_REQUESTED.swap(false, Ordering::SeqCst));
        // 收尾复位，防污染后续测试。
        RESTART_REQUESTED.store(false, Ordering::SeqCst);
    }

    /// v0.3.22（质检复核 P0-1）回归：主循环 spawn 子实例的命令必须
    /// 注入 RESTART_CHILD_ENV 环境标记——子实例凭它进入「等待旧实例
    /// 释放互斥体」路径；等待/接管/超时三段语义的断言在 single.rs
    /// （restart_child_waits_then_takes_over_or_times_out）。
    #[test]
    fn restart_spawn_carries_child_env_marker() {
        let cmd = super::imp::restart_command(std::path::Path::new("volttray-tray.exe"));
        let marker = cmd
            .get_envs()
            .find(|(k, _)| *k == std::ffi::OsStr::new(super::RESTART_CHILD_ENV));
        assert_eq!(
            marker.and_then(|(_, v)| v),
            Some(std::ffi::OsStr::new("1")),
            "重启 spawn 必须注入 RESTART_CHILD_ENV=1"
        );
    }
}

/// 菜单签名回归（原 imp::menu_sig_tests 随 menu_content_sig 移至 crate
/// root，使其在 Linux 侧可跑；既有用例语义零改动，仅按第 9 分量补参）。
#[cfg(test)]
mod menu_sig_tests {
    use super::*;

    fn dev(id: &str, link: &str) -> volttray_app::DeviceSnapshot {
        volttray_app::DeviceSnapshot {
            id: id.into(),
            name: id.into(),
            percent: Some(80),
            charging: false,
            link: link.into(),
            state: "online".into(),
            gray: false,
            source: None,
            physical_key: None,
            is_virtual: false,
            last_seen: None,
        }
    }

    /// R1-F2 锁定①：同分量输入 → 签名逐字节相等（不触发重建）。
    #[test]
    fn same_inputs_yield_same_sig() {
        let devices = vec![dev("m1", "btle")];
        let a = menu_content_sig(
            None,
            60,
            true,
            false,
            false,
            Some("m1"),
            &devices,
            false,
            false,
        );
        let b = menu_content_sig(
            None,
            60,
            true,
            false,
            false,
            Some("m1"),
            &devices,
            false,
            false,
        );
        assert_eq!(a, b);
    }

    /// R1-F2 锁定②（T-04③ 主场景）：其余分量不变、仅枚举失败摘要
    /// 翻转（新快照含失败候选）→ 签名不同 → 触发菜单重建，兜底提示行
    /// 随 rebuild 出现。修复前该布尔不进签名，主场景下兜底行不触发。
    #[test]
    fn enum_failure_flip_changes_sig() {
        let devices = vec![dev("m1", "btle")];
        let base = menu_content_sig(
            None,
            60,
            true,
            false,
            false,
            Some("m1"),
            &devices,
            false,
            false,
        );
        let with_failure = menu_content_sig(
            None,
            60,
            true,
            false,
            false,
            Some("m1"),
            &devices,
            true,
            false,
        );
        assert_ne!(base, with_failure);
    }

    /// R1-F2 锁定③：空设备列表下失败摘要翻转同样改变签名
    /// （兜底行恰在「无 direct 行」时出现，空列表是其典型现场）。
    #[test]
    fn enum_failure_flip_changes_sig_with_empty_devices() {
        let base = menu_content_sig(None, 60, false, false, false, None, &[], false, false);
        let with_failure = menu_content_sig(None, 60, false, false, false, None, &[], true, false);
        assert_ne!(base, with_failure);
    }

    /// 第 9 分量锁定①（本方案）：其余分量不变、仅充电快档实验开关
    /// 翻转 → 签名必变（FastPollState 三态随开关切换，文案须重建）。
    #[test]
    fn fast_poll_flag_flip_changes_sig() {
        let devices = vec![dev("m1", "btle")];
        let base = menu_content_sig(
            None,
            60,
            true,
            false,
            false,
            Some("m1"),
            &devices,
            false,
            false,
        );
        let with_fast = menu_content_sig(
            None,
            60,
            true,
            false,
            false,
            Some("m1"),
            &devices,
            false,
            true,
        );
        assert_ne!(base, with_fast);
    }

    /// 第 9 分量锁定②（本方案）：仅设备 charging 翻转 → 签名必变
    /// （charging 是 FastPollState 资格谓词的输入，菜单文案随之分态）。
    /// 既有回归同守：charging 本就进逐设备分量，此处锚定不回退。
    #[test]
    fn charging_flip_changes_sig() {
        let base_devs = vec![dev("m1", "btle")];
        let mut charging_dev = dev("m1", "btle");
        charging_dev.charging = true;
        let a = menu_content_sig(
            None,
            60,
            true,
            false,
            false,
            Some("m1"),
            &base_devs,
            false,
            false,
        );
        let b = menu_content_sig(
            None,
            60,
            true,
            false,
            false,
            Some("m1"),
            &[charging_dev],
            false,
            false,
        );
        assert_ne!(a, b);
    }
}
