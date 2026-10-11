//! 开机自启（写入边界 1：HKCU\...\Run 单值 "LogiVoltTray"）。
//! 非 Windows 平台提供同签名桩（返回 Unsupported），保证跨平台可编译。

pub const RUN_VALUE_NAME: &str = "LogiVoltTray";

#[cfg(windows)]
mod imp {
    use super::RUN_VALUE_NAME;
    use std::path::Path;
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ, REG_VALUE_TYPE,
    };

    const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn open_run_key(access: windows::Win32::System::Registry::REG_SAM_FLAGS) -> Result<HKEY, String> {
        let sub = wide(RUN_KEY);
        let mut hkey = HKEY::default();
        unsafe {
            let r = RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR::from_raw(sub.as_ptr()), None, access, &mut hkey);
            if r.is_err() {
                return Err(format!("RegOpenKeyExW failed: {r:?}"));
            }
        }
        Ok(hkey)
    }

    pub fn is_enabled() -> bool {
        let Ok(hkey) = open_run_key(KEY_QUERY_VALUE) else { return false };
        let name = wide(RUN_VALUE_NAME);
        let mut ty = REG_VALUE_TYPE(0);
        let mut size = 0u32;
        unsafe {
            let r = RegQueryValueExW(hkey, PCWSTR::from_raw(name.as_ptr()), None, Some(&mut ty), None, Some(&mut size));
            let _ = RegCloseKey(hkey);
            r.is_ok() && size > 0
        }
    }

    pub fn enable(exe_path: &Path) -> Result<(), String> {
        let hkey = open_run_key(KEY_SET_VALUE)?;
        let value = wide(&format!("\"{}\"", exe_path.display()));
        let bytes: &[u8] =
            unsafe { std::slice::from_raw_parts(value.as_ptr() as *const u8, value.len() * 2) };
        let name = wide(RUN_VALUE_NAME);
        unsafe {
            let r = RegSetValueExW(hkey, PCWSTR::from_raw(name.as_ptr()), None, REG_SZ, Some(bytes));
            let _ = RegCloseKey(hkey);
            if r.is_err() {
                return Err(format!("RegSetValueExW failed: {r:?}"));
            }
        }
        Ok(())
    }

    pub fn disable() -> Result<(), String> {
        let hkey = open_run_key(KEY_SET_VALUE)?;
        let name = wide(RUN_VALUE_NAME);
        unsafe {
            let r = RegDeleteValueW(hkey, PCWSTR::from_raw(name.as_ptr()));
            let _ = RegCloseKey(hkey);
            // 值不存在不算失败（幂等）。
            if r.is_err() && r.0 != 2 {
                return Err(format!("RegDeleteValueW failed: {r:?}"));
            }
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::Path;
    const UNSUPPORTED: &str = "autostart is only supported on Windows";
    pub fn is_enabled() -> bool {
        false
    }
    pub fn enable(_exe_path: &Path) -> Result<(), String> {
        Err(UNSUPPORTED.into())
    }
    pub fn disable() -> Result<(), String> {
        Err(UNSUPPORTED.into())
    }
}

pub use imp::{disable, enable, is_enabled};
