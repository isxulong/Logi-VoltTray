//! 托盘右键菜单（PRD M1：≤8 项；P2-3：设备区行不计入 8 项上限）。
//!
//! 固定项（5 项 + 分隔符）：立即刷新 / 诊断面板… / 开机自启 / 静默通知 / 退出。
//! 设备区：每台设备一行 CheckMenuItem，点击可将其钉为默认展示设备。

use tray_icon::menu::{CheckMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem};
use volttray_app::DeviceSnapshot;

/// 菜单项 ID 集合（构建时从 muda 取回，事件匹配用）。
pub struct MenuIds {
    pub device_rows: Vec<(MenuId, String)>,
    pub refresh: MenuId,
    pub diag: MenuId,
    pub autostart: MenuId,
    pub silent: MenuId,
    pub exit: MenuId,
}

fn device_row_text(dev: &DeviceSnapshot) -> String {
    let status = if dev.gray {
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

pub fn build_menu(
    devices: &[DeviceSnapshot],
    default_device: Option<&str>,
    pinned: Option<&str>,
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

    let refresh = MenuItem::new("立即刷新", true, None);
    let diag = MenuItem::new("诊断面板…", true, None);
    let autostart_item = CheckMenuItem::new("开机自启", true, autostart, None);
    let silent_item = CheckMenuItem::new("静默通知", true, silent, None);
    let exit = MenuItem::new("退出", true, None);

    let ids = MenuIds {
        device_rows,
        refresh: refresh.id().clone(),
        diag: diag.id().clone(),
        autostart: autostart_item.id().clone(),
        silent: silent_item.id().clone(),
        exit: exit.id().clone(),
    };

    let _ = menu.append(&refresh);
    let _ = menu.append(&diag);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&autostart_item);
    let _ = menu.append(&silent_item);
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&exit);
    (menu, ids)
}
