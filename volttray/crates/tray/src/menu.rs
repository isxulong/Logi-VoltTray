//! 托盘右键菜单（v0.3.0 产品方案 M13：固定项 7 个 ≤ 8；设备区行不计入上限）。
//!
//! 结构：设备区（每台一行 CheckMenuItem，点击钉为默认展示）/
//! ── / 立即刷新（当前间隔 {n} 秒）/ 低电闪烁提醒✓ / 通知静音✓ / 开机自启✓ /
//! ── / 设置… / 诊断面板… / ── / 退出。

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
    pub exit: MenuId,
}

fn device_row_text(dev: &DeviceSnapshot) -> String {
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
        let item = CheckMenuItem::new(device_row_text(dev), !dev.gray, checked, None);
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
    let diag = MenuItem::new("诊断面板…", true, None);
    let exit = MenuItem::new("退出", true, None);

    let ids = MenuIds {
        device_rows,
        refresh: refresh.id().clone(),
        blink: blink_item.id().clone(),
        silent: silent_item.id().clone(),
        autostart: autostart_item.id().clone(),
        settings: settings.id().clone(),
        diag: diag.id().clone(),
        exit: exit.id().clone(),
    };

    let _ = menu.append(&refresh);
    let _ = menu.append(&blink_item);
    let _ = menu.append(&silent_item);
    let _ = menu.append(&autostart_item);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&settings);
    let _ = menu.append(&diag);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&exit);
    (menu, ids)
}
