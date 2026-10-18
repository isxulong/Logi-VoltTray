//! 被动唤醒监听（D-33：零鼠标耗电方案）。
//!
//! 为什么是 Raw Input 而不是「读接收器的设备输入报告」：
//! - 配对鼠标经接收器对 Windows 呈现为系统 HID 鼠标，其输入接口由系统
//!   HID 类驱动独占，第三方进程拿不到该接口的报告流；
//! - 接收器 HID++ 接口上的自发通知（0x41 等）依固件而定、不保证覆盖
//!   「设备刚醒」这一事件；
//! - Raw Input（RIDEV_INPUTSINK + message-only 窗口）让我们以纯接收方式
//!   拿到全局鼠标活动事件——不向设备发任何查询，零射频成本。
//!
//! 语义：`is_logitech_mouse_input` 为 true 表示「系统观测到罗技鼠标活动」
//! （设备名含 VID_046D）；取不到设备名时保守返回 true——宁可多唤醒一次
//! （成本=一次电量查询），也不漏唤醒。

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::{
    GetRawInputData, GetRawInputDeviceInfoW, RegisterRawInputDevices, HRAWINPUT, RAWINPUTDEVICE,
    RAWINPUTHEADER, RIDEV_INPUTSINK, RIDI_DEVICENAME, RID_HEADER, RID_INPUT, RIM_TYPEMOUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, RegisterClassW, WINDOW_EX_STYLE, WINDOW_STYLE, WNDCLASSW,
    WNDCLASS_STYLES,
};

/// 罗技 USB Vendor ID（大小写不敏感匹配设备路径中的 VID_046D）。
const LOGI_VID: &str = "VID_046D";

/// Raw Input 接收窗口守卫：Drop 时销毁窗口（注册随窗口失效）。
pub struct RawInputSink {
    hwnd: HWND,
}

impl Drop for RawInputSink {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::DestroyWindow(self.hwnd);
        }
    }
}

unsafe extern "system" fn sink_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // WM_INPUT 在 GUI 消息泵里就地处理（main.rs），这里只走默认过程。
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// 安装被动监听：创建 message-only 窗口并注册鼠标 Raw Input（INPUTSINK）。
/// 失败返回 None——被动唤醒退化为「仅正常轮询节奏」（绝不主动保活）。
pub fn install() -> Option<RawInputSink> {
    unsafe {
        let hmodule = GetModuleHandleW(None).ok()?;
        let hinst = HINSTANCE(hmodule.0);
        let class: Vec<u16> = "VoltTrayRawInputSink"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let wc = WNDCLASSW {
            style: WNDCLASS_STYLES(0),
            lpfnWndProc: Some(sink_wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: windows::Win32::UI::WindowsAndMessaging::HICON::default(),
            hCursor: windows::Win32::UI::WindowsAndMessaging::HCURSOR::default(),
            hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH::default(),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: PCWSTR(class.as_ptr()),
        };
        // 类可能已注册（理论上单实例下不会；忽略重复注册错误）。
        let _ = RegisterClassW(&wc);

        // HWND_MESSAGE(-3)：message-only 窗口，不可见、不接收广播。
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(class.as_ptr()),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND(-3isize as *mut core::ffi::c_void)),
            None,
            Some(hinst),
            None,
        )
        .ok()?;

        let rid = RAWINPUTDEVICE {
            usUsagePage: 0x01,        // Generic Desktop
            usUsage: 0x02,            // Mouse
            dwFlags: RIDEV_INPUTSINK, // 后台也接收（不要求前台焦点）
            hwndTarget: hwnd,
        };
        RegisterRawInputDevices(
            std::slice::from_ref(&rid),
            std::mem::size_of::<RAWINPUTDEVICE>() as u32,
        )
        .ok()?;

        Some(RawInputSink { hwnd })
    }
}

/// 判定一条 WM_INPUT（lparam = HRAWINPUT）是否来自罗技鼠标。
///
/// 只读不写：GetRawInputData / GetRawInputDeviceInfoW 均为纯接收侧查询，
/// 不产生任何 HID++ 射线流量。
pub fn is_logitech_mouse_input(lparam: LPARAM) -> bool {
    unsafe {
        // 对齐缓冲（RAWINPUT 要求指针自然对齐；鼠标报文 ≤128B 足够）。
        let mut buf = [0u64; 16];
        let mut size = (buf.len() * std::mem::size_of::<u64>()) as u32;
        let got = GetRawInputData(
            HRAWINPUT(lparam.0 as *mut core::ffi::c_void),
            RID_INPUT,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            &mut size,
            std::mem::size_of::<RAWINPUTHEADER>() as u32,
        );
        if got == u32::MAX || got == 0 || size as usize > buf.len() * std::mem::size_of::<u64>() {
            // 读不到内容：保守视为可能相关（漏唤醒代价 > 多唤醒代价）。
            return true;
        }
        let header = &*(buf.as_ptr() as *const RAWINPUTHEADER);
        if header.dwType != RIM_TYPEMOUSE.0 {
            return false; // 键盘等其它设备输入：不相关。
        }
        // 设备名取不到：保守视为相关。
        let dev = header.hDevice;
        if dev.is_invalid() {
            return true;
        }
        let mut n: u32 = 0;
        let rc = GetRawInputDeviceInfoW(Some(dev), RIDI_DEVICENAME, None, &mut n);
        if rc == u32::MAX || n == 0 {
            return true;
        }
        let mut name = vec![0u16; n as usize];
        let rc = GetRawInputDeviceInfoW(
            Some(dev),
            RIDI_DEVICENAME,
            Some(name.as_mut_ptr() as *mut core::ffi::c_void),
            &mut n,
        );
        if rc == u32::MAX {
            return true;
        }
        let s = String::from_utf16_lossy(&name[..(n as usize).min(name.len())])
            .trim_end_matches('\0')
            .to_uppercase();
        s.contains(LOGI_VID)
    }
}

/// WP-A（v3 A-3 通道归因）：取触发本消息的 Raw Input 设备接口路径
/// （RIDI_DEVICENAME，两段式调用）。任一环节失败返回 None——调用方
/// 回退全量装填并记 channel_map_miss，不影响唤醒功能本身。
#[cfg(windows)]
pub fn device_path(lparam: LPARAM) -> Option<String> {
    unsafe {
        let mut hsize: u32 = 0;
        if GetRawInputData(
            HRAWINPUT(lparam.0 as *mut core::ffi::c_void),
            RID_HEADER,
            None,
            &mut hsize,
            std::mem::size_of::<RAWINPUTHEADER>() as u32,
        ) != 0
        {
            return None;
        }
        let mut hbuf = vec![0u8; hsize as usize];
        if GetRawInputData(
            HRAWINPUT(lparam.0 as *mut core::ffi::c_void),
            RID_HEADER,
            Some(hbuf.as_mut_ptr() as *mut core::ffi::c_void),
            &mut hsize,
            std::mem::size_of::<RAWINPUTHEADER>() as u32,
        ) != hsize
        {
            return None;
        }
        let header = &*(hbuf.as_ptr() as *const RAWINPUTHEADER);
        let mut nsize: u32 = 0;
        GetRawInputDeviceInfoW(Some(header.hDevice), RIDI_DEVICENAME, None, &mut nsize);
        if nsize == 0 {
            return None;
        }
        let mut nbuf = vec![0u16; nsize as usize];
        let got = GetRawInputDeviceInfoW(
            Some(header.hDevice),
            RIDI_DEVICENAME,
            Some(nbuf.as_mut_ptr() as *mut core::ffi::c_void),
            &mut nsize,
        );
        if got == 0 || got == u32::MAX {
            return None;
        }
        let end = nbuf.iter().position(|&c| c == 0).unwrap_or(nbuf.len());
        Some(String::from_utf16_lossy(&nbuf[..end]))
    }
}
