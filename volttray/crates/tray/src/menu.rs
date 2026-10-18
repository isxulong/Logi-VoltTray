//! 托盘右键菜单（v0.3.0 产品方案 M13：固定项 7 个 ≤ 8；设备区行不计入上限）。
//!
//! 结构：设备区（每台一行 CheckMenuItem，点击钉为默认展示）/
//! ── / 立即刷新（当前间隔 {n} 秒）/ 低电闪烁提醒✓ / 通知静音✓ / 开机自启✓ /
//! ── / 设置… / 导出诊断信息… / ── / 退出。

use tray_icon::menu::{CheckMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem};
use volttray_app::DeviceSnapshot;

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
        return format!("{} — {}", dev.name, status);
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
    format!("{} — {}", dev.name, status)
}

/// 构建菜单。
///
/// - `poll_secs`：当前轮询间隔，用于「立即刷新（当前间隔 {n} 秒）」文案（M10 可见可调）；
/// - `blink`：低电闪烁提醒开关（M11，菜单可直接关）；
/// - `autostart` / `silent`：沿用 v0.2.0 开关。
pub fn build_menu(
    devices: &[DeviceSnapshot],
    default_device: Option<&str>,
    pinned: Option<&str>,
    poll_secs: u64,
    blink: bool,
    autostart: bool,
    silent: bool,
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

    let refresh = MenuItem::new(format!("立即刷新（当前间隔 {poll_secs} 秒）"), true, None);
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
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&exit);
    (menu, ids)
}
