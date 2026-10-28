//! 托盘右键菜单（T-07 起固定项 9 个：超出 v0.3.0 产品方案 M13 原上限 8，
//! 实施令 2026-10-20 注记「后续再加固定项须先评审」进对账表 F.2；
//! 设备区行不计入额度）。
//!
//! 结构：设备区（每台一行 CheckMenuItem，点击钉为默认展示）/
//! ── / 立即刷新（当前间隔 {人性化单位，如 30 分钟}；实验快档生效时
//! 按充电三态分文案——统一充电形「15 秒·充电中，常规 …」/ 混合形
//! 「… · 充电快档 15 秒」，见 settings_text::menu_refresh_label）/
//! 低电闪烁提醒✓ / 通知静音✓ / 开机自启✓ /
//! ── / 设置… / 导出诊断信息… / 导出耗电统计… / 检查更新（当前 v{X}）/
//! ── / 退出。

use tray_icon::menu::{CheckMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem};
use volttray_app::settings_text::menu_refresh_label;
use volttray_app::{DeviceSnapshot, FastPollState};

/// 菜单项 ID 集合（构建时从 muda 取回，事件匹配用）。
pub struct MenuIds {
    pub device_rows: Vec<(MenuId, String)>,
    pub refresh: MenuId,
    pub blink: MenuId,
    pub silent: MenuId,
    pub autostart: MenuId,
    pub settings: MenuId,
    pub diag: MenuId,
    /// WP-B：导出耗电统计（CSV + 可选 HTML 报告 / JSON）。
    pub stats_export: MenuId,
    /// T-07：手动「检查更新」——签发口径零网络形态：事件循环仅
    /// ShellExecuteW 打开 GitHub /releases/latest 页，无 HTTP 客户端、
    /// 零常驻外联、零启动自动检查。
    pub check_update: MenuId,
    pub exit: MenuId,
}

fn device_row_text(dev: &DeviceSnapshot) -> String {
    // WP-A（A-1b）：虚拟条目——重启后来自 last_known.json 的最后已知读数，
    // 灰显 + 更新时刻注释；与真实条目「休眠中」文案区分，不冒充新读数。
    if dev.is_virtual {
        let status = match (dev.percent, dev.last_seen) {
            (Some(p), Some(t)) => {
                format!("{p}% 最后已知 · 更新于 {}", crate::timelocal::fmt_hhmm(t))
            }
            (Some(p), None) => format!("{p}% 最后已知"),
            (None, Some(t)) => {
                format!(
                    "最后已知 · 更新于 {} · 电量未知",
                    crate::timelocal::fmt_hhmm(t)
                )
            }
            (None, None) => "最后已知 · 电量未知".to_string(),
        };
        return format!("{} — {}{}", dev.name, status, direct_annotation(dev));
    }
    // D-33：休眠是合法状态——显示最后已知电量并注明休眠，绝不显示「未读取/未知」。
    let status = if dev.state == "sleeping" {
        match dev.percent {
            Some(p) => format!("{p}% 休眠中"),
            None => "休眠中 · 电量未知".to_string(),
        }
    } else if dev.gray {
        "离线".to_string()
    } else {
        match dev.percent {
            Some(p) if dev.charging => format!("{p}% 充电中"),
            Some(p) => format!("{p}%"),
            None => "电量未知".to_string(),
        }
    };
    format!("{} — {}{}", dev.name, status, direct_annotation(dev))
}

/// T-04（C-F1 宽标注）：直连（蓝牙/有线）通道读不到读数（`direct &&
/// (percent 无值 || 灰显)`）时的行尾标注。sleeping 行既有文案已含
/// 「休眠中」、不引导导出（F-B3 文案分流，误标无效反馈清零）；其余
/// direct 失败行（含虚拟最后已知行）统一标注引导语，把必然发生的
/// 蓝牙首跑失败转化为反馈漏斗入口。设备行点击语义不变（钉选 +
/// refresh_ui，零查询）。
fn direct_annotation(dev: &DeviceSnapshot) -> &'static str {
    if dev.link != "direct" || !(dev.percent.is_none() || dev.gray) {
        return "";
    }
    if dev.state == "sleeping" {
        ""
    } else {
        "（该通道未验证，读不到请用『导出诊断信息…』）"
    }
}

/// T-04③ 判定：无 direct 设备行、且最近一次枚举快照存在打开失败的
/// 蓝牙（BTHHID/BTHENUM）候选通道时，兜底提示行成立。判定复用 IO 侧
/// 既有 key/tag 信息（Windows 蓝牙 HID 路径 tag 含 bthhid/bthenum 段），
/// GUI 侧零设备查询。
/// R1-F2 抽出为共享判定——main.rs 把它并入菜单内容签名，与重建分支的
/// 追加判定同源，两处永不发散。
pub fn enum_failure_hint_applies(
    devices: &[DeviceSnapshot],
    enum_snap: Option<&volttray_app::diag_export::EnumSnapshot>,
) -> bool {
    if devices.iter().any(|d| d.link == "direct") {
        return false;
    }
    let Some(snap) = enum_snap else {
        return false;
    };
    snap.channels.iter().any(|c| {
        let t = c.tag.to_ascii_lowercase();
        (t.contains("bthhid") || t.contains("bthenum"))
            && (c.open == "failed" || c.open == "cached-reopen-failed")
    })
}

/// T-04③：判定成立时追加一条禁用态信息行（不进 MenuIds、零事件
/// 分支——M13 固定项额度不含设备区行）。
pub fn append_direct_enum_failure_hint(
    menu: &Menu,
    devices: &[DeviceSnapshot],
    enum_snap: Option<&volttray_app::diag_export::EnumSnapshot>,
) {
    if !enum_failure_hint_applies(devices, enum_snap) {
        return;
    }
    let item = MenuItem::new("蓝牙/有线通道识别失败 → 请导出诊断反馈", false, None);
    let _ = menu.append(&item);
}

/// 构建菜单。
///
/// - `poll_secs`：当前轮询间隔（秒），用于「立即刷新（当前间隔 …）」文案
///   （M10 可见可调；显示层经 humanize_interval_secs 换算为分钟/小时）；
/// - `fast_state`：充电快档三态（FastPollState；实验快档生效且存在
///   符合资格的充电设备时「立即刷新」项分三态文案，见
///   settings_text::menu_refresh_label——业务判定在 AppCore，此处只收
///   已判定的状态）；
/// - `blink`：低电闪烁提醒开关（M11，菜单可直接关）；
/// - `autostart` / `silent`：沿用 v0.2.0 开关。
#[allow(clippy::too_many_arguments)]
pub fn build_menu(
    devices: &[DeviceSnapshot],
    default_device: Option<&str>,
    pinned: Option<&str>,
    poll_secs: u64,
    blink: bool,
    autostart: bool,
    silent: bool,
    fast_state: FastPollState,
) -> (Menu, MenuIds) {
    let menu = Menu::new();
    let mut device_rows = Vec::new();

    for dev in devices {
        let checked = pinned.map_or(default_device == Some(dev.id.as_str()), |p| p == dev.id);
        // WP-A（v3 A-2）：可展示数据即可点（含休眠灰行与虚拟最后已知行）；
        // 点击语义不变（pin + refresh_ui，零查询）。无任何读数才禁用。
        let item = CheckMenuItem::new(
            device_row_text(dev),
            dev.has_displayable_data(),
            checked,
            None,
        );
        device_rows.push((item.id().clone(), dev.id.clone()));
        let _ = menu.append(&item);
    }
    if !devices.is_empty() {
        let _ = menu.append(&PredefinedMenuItem::separator());
    }

    // 2026-10-25 用户反馈：秒不易读——间隔显示改人性化单位（分钟/小时，
    // 换算规则见 settings_text::humanize_interval_secs）；显示层专用，
    // poll_secs 数值与配置存储保持秒不变。
    // FR-7（RB2）：按配置真值显示，非 POLL_TIERS_SECS 档位值追加
    // 「（非标准档）」（settings_text::menu_poll_interval_label），避免
    // 手改 config 为档外值时菜单与设置窗口下拉选中档文案矛盾。
    // 充电三态：实验快档生效时按 fast_state 分三态文案
    // （settings_text::menu_refresh_label；FR-7 例外条款登记于该函数
    // 文档注释——配置真值以「常规 …」右注逐字保留）。
    let refresh = MenuItem::new(menu_refresh_label(poll_secs, fast_state), true, None);
    let blink_item = CheckMenuItem::new("低电闪烁提醒", true, blink, None);
    // v0.3.14：改名澄清语义——该开关实际只控制 toast 提示音
    // （toast.rs 的 <audio silent="true"/>），既不抑制弹通知本身，
    // 也与图标闪烁（blink_enabled 管辖）无关。原名「静默通知」让用户
    // 误以为它是「所有提醒的总开关」，是现象 1 的认知层根因。
    // 配置键 notification_silent 保持不变（向后兼容，无迁移）。
    let silent_item = CheckMenuItem::new("通知静音（仅关提示音）", true, silent, None);
    let autostart_item = CheckMenuItem::new("开机自启", true, autostart, None);
    let settings = MenuItem::new("设置…", true, None);
    // R-7（v0.3.17 真机终审）：菜单文案与实际行为对齐——该项打开的是
    // 导出诊断信息的另存为流程，并非「面板」。
    let diag = MenuItem::new("导出诊断信息…", true, None);
    let stats_export = MenuItem::new("导出耗电统计…", true, None);
    // T-07（实施令零网络形态）：固定项「检查更新（当前 v{X}）」。实施令
    // 口径为 7→8（M13 顶格 8/8），但现实基线固定项已是 8（stats_export 已
    // 随 v0.3.19 在列），本项使固定项达 9、突破 M13 原上限 8——计数按现实
    // 断言（见下方单测），偏离与「后续再加固定项须先评审」注记均登记于
    // 对账表 F.2。版本号取编译期 CARGO_PKG_VERSION，与 EXE 内嵌版本资源
    // 同源（门禁 B3 防手工脱节）。
    let check_update = MenuItem::new(
        format!("检查更新（当前 v{}）", env!("CARGO_PKG_VERSION")),
        true,
        None,
    );
    let exit = MenuItem::new("退出", true, None);

    let ids = MenuIds {
        device_rows,
        refresh: refresh.id().clone(),
        blink: blink_item.id().clone(),
        silent: silent_item.id().clone(),
        autostart: autostart_item.id().clone(),
        settings: settings.id().clone(),
        diag: diag.id().clone(),
        stats_export: stats_export.id().clone(),
        check_update: check_update.id().clone(),
        exit: exit.id().clone(),
    };

    let _ = menu.append(&refresh);
    let _ = menu.append(&blink_item);
    let _ = menu.append(&silent_item);
    let _ = menu.append(&autostart_item);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&settings);
    let _ = menu.append(&diag);
    let _ = menu.append(&stats_export);
    let _ = menu.append(&check_update);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&exit);
    (menu, ids)
}

#[cfg(test)]
mod tests {
    //! T-04 验收补强（F-A1 四断言）＋兜底信息行断言。menu.rs 此前无既有
    //! 测试，本模块为纯新增。menu 依赖 tray-icon，测试由 Windows CI 承载。
    use super::*;

    fn snap(
        id: &str,
        name: &str,
        link: &str,
        percent: Option<u8>,
        state: &str,
        gray: bool,
        is_virtual: bool,
    ) -> DeviceSnapshot {
        DeviceSnapshot {
            id: id.to_string(),
            name: name.to_string(),
            percent,
            charging: false,
            link: link.to_string(),
            state: state.to_string(),
            gray,
            source: Some("test".to_string()),
            physical_key: Some("k".to_string()),
            is_virtual,
            last_seen: None,
        }
    }

    /// 断言一：direct + percent=None 必含标注（C-F1 宽条件）。
    #[test]
    fn direct_no_percent_row_contains_annotation() {
        let dev = snap("d1", "MX Keys Mini", "direct", None, "active", true, false);
        assert!(
            device_row_text(&dev).contains("（该通道未验证，读不到请用『导出诊断信息…』）"),
            "direct 读不到读数必须含标注：{}",
            device_row_text(&dev)
        );
    }

    /// 断言二：unifying 正常设备不含标注。
    #[test]
    fn unifying_normal_row_has_no_annotation() {
        let dev = snap(
            "u1",
            "MX Master 3S",
            "unifying",
            Some(55),
            "active",
            false,
            false,
        );
        let text = device_row_text(&dev);
        assert!(!text.contains("未验证"), "已验证通道不标注：{text}");
        assert!(text.contains("55%"), "正常电量照常显示：{text}");
    }

    /// 断言三：direct + sleeping 含「休眠中」（文案分流，不引导导出）。
    #[test]
    fn direct_sleeping_row_says_sleeping_no_export_hint() {
        let dev = snap(
            "b1",
            "MX Anywhere 3",
            "direct",
            None,
            "sleeping",
            true,
            false,
        );
        let text = device_row_text(&dev);
        assert!(text.contains("休眠中"), "sleeping 行含「休眠中」：{text}");
        assert!(
            !text.contains("导出诊断信息"),
            "sleeping 行不引导导出：{text}"
        );
    }

    /// 断言四：direct + 虚拟最后已知行含标注。
    #[test]
    fn direct_virtual_row_contains_annotation() {
        let dev = snap("v1", "MX Keys Mini", "direct", None, "offline", true, true);
        let text = device_row_text(&dev);
        assert!(dev.is_virtual);
        assert!(
            text.contains("（该通道未验证，读不到请用『导出诊断信息…』）"),
            "{text}"
        );
    }

    /// 兜底信息行：无 direct 行且快照含 BTHHID 失败候选时追加禁用态行。
    #[test]
    fn hint_row_appended_when_bt_candidate_failed() {
        use volttray_app::diag_export::{ChannelEnumResult, EnumSnapshot};
        let menu = Menu::new();
        let devs = vec![snap(
            "u1",
            "MX Master 3S",
            "unifying",
            Some(80),
            "active",
            false,
            false,
        )];
        let snap_enum = EnumSnapshot {
            at_ms: 1,
            forced: false,
            candidate_channels: 2,
            channels: vec![
                ChannelEnumResult {
                    key: "C52B|vid_046d".to_string(),
                    pid: "0xC52B".to_string(),
                    tag: "vid_046d&pid_c52b".to_string(),
                    open: "ok".to_string(),
                    error: None,
                },
                ChannelEnumResult {
                    key: "0001|bthhid".to_string(),
                    pid: "0x0001".to_string(),
                    tag: "bthhid&dev_...".to_string(),
                    open: "failed".to_string(),
                    error: Some("hid open failed".to_string()),
                },
            ],
            registered_slots: 1,
            error: None,
        };
        append_direct_enum_failure_hint(&menu, &devs, Some(&snap_enum));
        assert_eq!(menu.items().len(), 1, "追加一条禁用态信息行");
    }

    /// 兜底信息行：存在 direct 设备行时不追加。
    #[test]
    fn hint_row_skipped_when_direct_device_present() {
        let menu = Menu::new();
        let devs = vec![snap(
            "d1",
            "MX Keys Mini",
            "direct",
            Some(70),
            "active",
            false,
            false,
        )];
        append_direct_enum_failure_hint(&menu, &devs, None);
        assert_eq!(menu.items().len(), 0, "有 direct 行不追加");
    }
}

/// R2-F4：T-07 的两条断言原混在 T-04 新建的 tests 模块里——T-04 若需
/// 单独 revert 会连带删掉 T-07 的测试。移出为独立模块，恢复 T-04 单独
/// revert 的独立性（T-07 验收断言：固定项计数＋版本文案；断言内容逐字
/// 保留。menu 依赖 tray-icon，测试由 Windows CI 承载）。
#[cfg(test)]
mod t07_tests {
    use super::*;

    /// T-07：固定项计数断言——MenuItem / CheckMenuItem 合计 9 个（分隔符
    /// 与设备区行不计入 M13 额度）。实施令口径写「7→8」，现实基线固定项
    /// 已是 8（stats_export 随 v0.3.19 在列），故本项后为 9、突破 M13 原上
    /// 限 8——按现实断言，偏离登记于对账表 F.2（测试由 Windows CI 承载）。
    #[test]
    fn t07_fixed_item_count_is_nine() {
        use tray_icon::menu::MenuItemKind;
        let (menu, _ids) = build_menu(&[], None, None, 1800, true, true, false, FastPollState::Off);
        let fixed = menu
            .items()
            .into_iter()
            .filter(|k| !matches!(k, MenuItemKind::Predefined(_)))
            .count();
        assert_eq!(
            fixed, 9,
            "固定项应为 9：refresh/blink/silent/autostart/settings/diag/stats_export/check_update/exit"
        );
    }

    /// T-07：新项文本含当前版本号（编译期 CARGO_PKG_VERSION，与 EXE 内嵌
    /// 版本资源同源，门禁 B3）。
    #[test]
    fn t07_check_update_item_text_contains_version() {
        use tray_icon::menu::MenuItemKind;
        let (menu, ids) = build_menu(&[], None, None, 1800, true, true, false, FastPollState::Off);
        let text = menu
            .items()
            .into_iter()
            .find_map(|k| match k {
                MenuItemKind::MenuItem(mi) if mi.id() == &ids.check_update => Some(mi.text()),
                _ => None,
            })
            .expect("check_update 项应存在于固定项区");
        assert!(
            text.starts_with("检查更新（当前 v"),
            "文案前缀须为「检查更新（当前 v」：{text}"
        );
        assert!(
            text.contains(env!("CARGO_PKG_VERSION")),
            "文案须含编译期版本号 {}：{text}",
            env!("CARGO_PKG_VERSION")
        );
    }

    /// 充电三态「立即刷新」项逐字文案（签发稿测试要点 5）：Off/Unified/
    /// Mixed 三态下 refresh 项文本与 settings_text::menu_refresh_label
    /// 输出逐字一致；固定项计数回归仍为 9——三态只改写既有 refresh 项
    /// 纯文案，不新增固定项（M13 上限纪律零破坏）。menu 依赖
    /// tray-icon，测试由 Windows CI 承载（本文件 cfg(windows) 门控，
    /// Linux 无法本地编译——属登记的未验证边界）。
    #[test]
    fn t07_refresh_item_text_follows_fast_poll_state() {
        use tray_icon::menu::MenuItemKind;
        let cases = [
            (FastPollState::Off, "立即刷新（当前间隔 30 分钟）"),
            (
                FastPollState::Unified,
                "立即刷新（当前间隔 15 秒·充电中，常规 30 分钟）",
            ),
            (
                FastPollState::Mixed,
                "立即刷新（当前间隔 30 分钟 · 充电快档 15 秒）",
            ),
        ];
        for (state, expected) in cases {
            let (menu, ids) = build_menu(&[], None, None, 1800, true, true, false, state);
            let text = menu
                .items()
                .into_iter()
                .find_map(|k| match k {
                    MenuItemKind::MenuItem(mi) if mi.id() == &ids.refresh => Some(mi.text()),
                    _ => None,
                })
                .expect("refresh 项应存在于固定项区");
            assert_eq!(text, expected, "三态 refresh 项逐字文案");
            // 固定项计数回归：三态不改变固定项个数（仍 9）。
            let fixed = menu
                .items()
                .into_iter()
                .filter(|k| !matches!(k, MenuItemKind::Predefined(_)))
                .count();
            assert_eq!(fixed, 9, "三态只改写 refresh 项文案，不新增固定项");
        }
    }
}
