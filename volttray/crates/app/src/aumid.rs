//! AUMID 与开始菜单快捷方式（评审 P0-5：第 4 个写入边界）。
//!
//! 首次运行时在开始菜单创建 LogiVoltTray.lnk，写入
//! System.AppUserModel.ID = "Logi.VoltTray"，使 WinRT toast 能正常投递。
//! 失败不致命：上层降级为灰图标 + tooltip 提示（P0-5 裁决）。
//! 非 Windows 平台提供同签名桩。

pub const AUMID: &str = "Logi.VoltTray";
pub const SHORTCUT_NAME: &str = "LogiVoltTray.lnk";

#[cfg(windows)]
mod imp {
    use super::{AUMID, SHORTCUT_NAME};
    use std::path::{Path, PathBuf};
    use windows::core::{Interface, GUID, PCWSTR, PWSTR};
    use windows::Win32::Foundation::PROPERTYKEY;
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, IPersistFile, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::System::Variant::VT_LPWSTR;
    use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
    use windows::Win32::UI::Shell::{
        IShellLinkW, SetCurrentProcessExplicitAppUserModelID, ShellLink,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// PKEY_AppUserModel_ID = {9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3}, pid=5
    fn pkey_aumid() -> PROPERTYKEY {
        PROPERTYKEY {
            fmtid: GUID::from_values(
                0x9F4C2855,
                0x9F79,
                0x4B39,
                [0xA8, 0xD0, 0xE1, 0xD4, 0x2D, 0xE1, 0xD5, 0xF3],
            ),
            pid: 5,
        }
    }

    pub fn shortcut_path() -> PathBuf {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("Microsoft")
            .join("Windows")
            .join("Start Menu")
            .join("Programs")
            .join(SHORTCUT_NAME)
    }

    pub fn set_process_aumid() -> Result<(), String> {
        let id = wide(AUMID);
        // SAFETY(E9): id 为 wide() 生成的 NUL 结尾 UTF-16 缓冲，调用期间存活；PCWSTR 仅借用其指针，不转移所有权。
        unsafe { SetCurrentProcessExplicitAppUserModelID(PCWSTR::from_raw(id.as_ptr())) }
            .map_err(|e| format!("SetCurrentProcessExplicitAppUserModelID failed: {e}"))
    }

    /// 快捷方式已存在则跳过；否则创建并写入 AUMID 属性。
    pub fn ensure_start_menu_shortcut(exe_path: &Path) -> Result<(), String> {
        let link_path = shortcut_path();
        if link_path.exists() {
            return Ok(());
        }
        if let Some(dir) = link_path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("mkdir failed: {e}"))?;
        }
        // SAFETY(E9): COM 以 COINIT_APARTMENTTHREADED 在本线程初始化（S_FALSE 亦接受）；COM 接口指针由 windows crate RAII 持有，不跨线程传递。
        unsafe {
            // S_FALSE（已初始化）也视为成功。
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| format!("CoCreateInstance(ShellLink) failed: {e}"))?;
            let exe = wide(&exe_path.display().to_string());
            link.SetPath(PCWSTR::from_raw(exe.as_ptr()))
                .map_err(|e| format!("SetPath failed: {e}"))?;
            let desc = wide("LogiVoltTray - battery monitor");
            let _ = link.SetDescription(PCWSTR::from_raw(desc.as_ptr()));

            // 写 AUMID 属性（手工构造 VT_LPWSTR PROPVARIANT，D-09）。
            let store: IPropertyStore = link
                .cast()
                .map_err(|e| format!("cast IPropertyStore failed: {e}"))?;
            let mut aumid_w = wide(AUMID);
            let pv = PROPVARIANT {
                Anonymous: windows::Win32::System::Com::StructuredStorage::PROPVARIANT_0 {
                    Anonymous: std::mem::ManuallyDrop::new(
                        windows::Win32::System::Com::StructuredStorage::PROPVARIANT_0_0 {
                            vt: VT_LPWSTR,
                            wReserved1: 0,
                            wReserved2: 0,
                            wReserved3: 0,
                            Anonymous:
                                windows::Win32::System::Com::StructuredStorage::PROPVARIANT_0_0_0 {
                                    pwszVal: PWSTR::from_raw(aumid_w.as_mut_ptr()),
                                },
                        },
                    ),
                },
            };
            let pkey = pkey_aumid();
            store
                .SetValue(&pkey, &pv)
                .map_err(|e| format!("IPropertyStore::SetValue failed: {e}"))?;
            store
                .Commit()
                .map_err(|e| format!("IPropertyStore::Commit failed: {e}"))?;

            let persist: IPersistFile = link
                .cast()
                .map_err(|e| format!("cast IPersistFile failed: {e}"))?;
            let path_w = wide(&link_path.display().to_string());
            persist
                .Save(PCWSTR::from_raw(path_w.as_ptr()), true)
                .map_err(|e| format!("IPersistFile::Save failed: {e}"))?;
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::Path;
    pub fn set_process_aumid() -> Result<(), String> {
        Err("AUMID is only supported on Windows".into())
    }
    pub fn ensure_start_menu_shortcut(_exe_path: &Path) -> Result<(), String> {
        Err("shortcut is only supported on Windows".into())
    }
}

pub use imp::{ensure_start_menu_shortcut, set_process_aumid};
