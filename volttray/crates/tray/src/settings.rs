//! 设置窗口（v0.3.0 产品方案 M9-M13）：裸 Win32 单页非模态窗口。
//!
//! 形态：无模式、单实例（重复打开只前置既有窗口）、改动即时生效（无确定/取消）、
//! 关闭即销毁。GUI 改动直接写入共享配置（AppCore 同一个 `Arc<RwLock<Config>>`）
//! 并持久化 TOML；TOML 侧改动经既有 mtime 热重载进入共享配置——双向一致。
//!
//! 三组控件 + 每组「恢复默认」+ 全局「恢复全部默认」：
//! - 外观：形状（圆角/直角/透明）、背景色（深/浅/自定义 #RRGGBB）、字体（系统
//!   字体枚举）、字体颜色（auto / 自定义 #RRGGBB）、实时预览；对比度 <3.0 仅警告
//!   不阻断（M9）；
//! - 刷新：轮询间隔 15/30/60/120/300 秒（M10）；
//! - 低电提醒：闪烁开关 + 阈值滑块 5..=50 步进 5（M11/M12）。

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, RwLock};

use volttray_app::config::{
    contrast_ok, contrast_ratio, Config, IconBackground, IconFontColor, IconShape, RgbColor,
    CONTRAST_FLOOR, DEFAULT_LOW_THRESHOLD, DEFAULT_POLL_SECS, LOW_THRESHOLD_MAX,
    LOW_THRESHOLD_MIN,
};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, DeleteObject, EnumFontFamiliesExW, GetDC, GetStockObject, ReleaseDC,
    DEFAULT_CHARSET, DEFAULT_GUI_FONT, HBRUSH, HDC, HFONT, HGDIOBJ, LOGFONTW, TEXTMETRICW,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, ICC_BAR_CLASSES, INITCOMMONCONTROLSEX, TBM_SETRANGE, TBM_SETPOS,
    TBM_SETTICFREQ, TBS_AUTOTICKS, BST_CHECKED, BST_UNCHECKED,
};
use windows::Win32::UI::WindowsAndMessaging::{
    BM_GETCHECK, BM_SETCHECK, BN_CLICKED, CB_ADDSTRING, CB_FINDSTRINGEXACT, CB_GETCOUNT,
    CB_GETCURSEL, CB_GETLBTEXT, CB_GETLBTEXTLEN, CB_SETCURSEL, CBS_DROPDOWNLIST, CBN_SELCHANGE,
    CREATESTRUCTW, CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, EN_CHANGE,
    ES_AUTOHSCROLL, GetDlgItem, GetWindowTextW, GetWindowLongPtrW, GWLP_USERDATA, HCURSOR, HICON,
    HMENU, IMAGE_BITMAP, IsWindow, RegisterClassW, SendMessageW, SetForegroundWindow, SetWindowTextW,
    SetWindowLongPtrW, ShowWindow, STM_SETIMAGE, SW_SHOW, WM_ACTIVATE, WM_CLOSE, WM_COMMAND,
    WM_CREATE, WM_DESTROY, WM_HSCROLL, WM_NCDESTROY, WM_SETFONT, WINDOW_EX_STYLE, WINDOW_STYLE,
    WNDCLASSW, WNDCLASS_STYLES, WS_CHILD, WS_EX_CLIENTEDGE, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
    WS_VSCROLL,
};

use crate::icon::{font_rgb, render_text_bitmap, IconColor};

/// SendMessageW 包装（Option 参数形态见 0.62 绑定；返回 LRESULT 内层值）。
macro_rules! SendMessageW {
    ($hwnd:expr, $msg:expr, $wp:expr, $lp:expr) => {
        SendMessageW($hwnd, $msg, Some($wp), Some($lp)).0
    };
}

// windows crate 0.62 未导出的两个控件常量（本地定义）：
// TBM_GETPOS = WM_USER+0；SS_BITMAP = 0x0E。
const TBM_GETPOS: u32 = 0x0400;
const SS_BITMAP: u32 = 14;

/// BS_GROUPBOX（分组框样式；windows crate 以 i32 导出，这里只用作样式位）。
const BS_GROUPBOX: u32 = 7;
/// BS_AUTOCHECKBOX。
const BS_AUTOCHECKBOX: u32 = 3;

/// 设置窗口单实例句柄（0 = 未打开）。
static SETTINGS_HWND: AtomicIsize = AtomicIsize::new(0);

// 控件 ID。
const IDC_SHAPE: i32 = 2001;
const IDC_BG: i32 = 2002;
const IDC_BG_HEX: i32 = 2003;
const IDC_FONT_FAMILY: i32 = 2004;
const IDC_FONT_COLOR: i32 = 2005;
const IDC_FONT_HEX: i32 = 2006;
const IDC_PREVIEW: i32 = 2007;
const IDC_RESET_APPEAR: i32 = 2008;
const IDC_RESET_REFRESH: i32 = 2009;
const IDC_RESET_LOW: i32 = 2010;
const IDC_RESET_ALL: i32 = 2011;
const IDC_BLINK: i32 = 2012;
const IDC_SLIDER: i32 = 2013;
const IDC_THRESH_VAL: i32 = 2014;
const IDC_STATUS: i32 = 2015;
const IDC_INTERVAL: i32 = 2016;

/// 预览位图边长（像素）。
const PREVIEW_PX: i32 = 48;

const INTERVALS: [u64; 5] = [15, 30, 60, 120, 300];

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide_slice(s: &[u16]) -> String {
    let len = s.iter().position(|c| *c == 0).unwrap_or(s.len());
    String::from_utf16_lossy(&s[..len])
}

/// 窗口上下文（WM_CREATE 起挂在 GWLP_USERDATA 上，销毁时释放）。
struct Ctx {
    /// 工作配置副本（控件 ↔ 配置的编辑缓冲）。
    cfg: RefCell<Config>,
    /// 共享配置（与 AppCore 同一 Arc；写入即时生效）。
    arc: Arc<RwLock<Config>>,
    /// 持久化路径。
    path: PathBuf,
    dark_theme: bool,
    /// 程序化同步控件时抑制自身事件。
    syncing: Cell<bool>,
    /// 预览文本（如当前电量数字）。
    preview_text: String,
}

impl Ctx {
    /// 即时生效：写入共享配置 + 持久化 TOML（M9：无确定/取消）。
    fn apply(&self, cfg: &Config) {
        if let Ok(mut guard) = self.arc.write() {
            *guard = cfg.clone();
        }
        let _ = std::fs::write(&self.path, cfg.to_toml());
    }
}

/// 打开（或前置）设置窗口。`preview_text` 为预览数字（如当前电量文本）。
pub fn open_settings(arc: &Arc<RwLock<Config>>, path: &Path, dark_theme: bool, preview_text: &str) {
    let existing = SETTINGS_HWND.load(Ordering::SeqCst);
    if existing != 0 {
        let hwnd = HWND(existing as *mut core::ffi::c_void);
        unsafe {
            if IsWindow(Some(hwnd)).as_bool() {
                let _ = SetForegroundWindow(hwnd);
                return;
            }
        }
    }

    unsafe {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_BAR_CLASSES,
        };
        let _ = InitCommonControlsEx(&icc);

        let hinst = match GetModuleHandleW(None) {
            Ok(h) => windows::Win32::Foundation::HINSTANCE(h.0),
            Err(_) => return,
        };
        let class = wide("VoltTraySettingsWnd");
        let title = wide("VoltTray 设置");
        let wc = WNDCLASSW {
            style: WNDCLASS_STYLES(0),
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: HICON::default(),
            hCursor: HCURSOR::default(),
            hbrBackground: HBRUSH(6usize as *mut core::ffi::c_void), // COLOR_WINDOW + 1
            lpszMenuName: PCWSTR::null(),
            lpszClassName: PCWSTR(class.as_ptr()),
        };
        // 类可能已注册（重复打开）：忽略「类已存在」错误。
        let _ = RegisterClassW(&wc);

        let ctx = Box::new(Ctx {
            cfg: RefCell::new(arc.read().map(|c| c.clone()).unwrap_or_default()),
            arc: arc.clone(),
            path: path.to_path_buf(),
            dark_theme,
            syncing: Cell::new(false),
            preview_text: preview_text.to_string(),
        });

        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            510,
            570,
            None,
            None,
            Some(hinst),
            Some(Box::into_raw(ctx) as *const core::ffi::c_void),
        ) {
            Ok(h) => h,
            Err(_) => return,
        };
        SETTINGS_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
}

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let ctx = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Ctx;
    match msg {
        WM_CREATE => {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            let raw = cs.lpCreateParams;
            let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);
            let ctx = &*(raw as *const Ctx);
            build_controls(hwnd, ctx);
            LRESULT(0)
        }
        WM_ACTIVATE => {
            // 双向一致（M9）：窗口重新激活时以共享配置为准（覆盖外部 TOML 改动
            // 后的控件状态）。仅在「激活」而非每次点击时同步。
            if wparam.0 & 0xFFFF == 1 && !ctx.is_null() {
                let ctx = &*ctx;
                if let Ok(guard) = ctx.arc.read() {
                    *ctx.cfg.borrow_mut() = guard.clone();
                    sync_controls(hwnd, ctx);
                }
            }
            LRESULT(0)
        }
        WM_HSCROLL => {
            // 阈值滑块（发给父窗口；lparam = 滑块 HWND）。
            if !ctx.is_null() {
                let ctx = &*ctx;
                let slider = GetDlgItem(Some(hwnd), IDC_SLIDER).unwrap_or_default();
                if slider.0 == lparam.0 as *mut core::ffi::c_void && !ctx.syncing.get() {
                    let pos = SendMessageW!(slider, TBM_GETPOS, WPARAM(0), LPARAM(0)) as i32;
                    let snapped = ((pos as f64 / 5.0).round() * 5.0) as i64;
                    let v = snapped.clamp(LOW_THRESHOLD_MIN as i64, LOW_THRESHOLD_MAX as i64) as u8;
                    let mut cfg = ctx.cfg.borrow_mut();
                    if cfg.low_threshold != v {
                        cfg.low_threshold = v;
                        let text = format!("{}%", v);
                        set_text(ctx, IDC_THRESH_VAL, &text);
                        ctx.apply(&cfg);
                    }
                }
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as i32;
            let code = (wparam.0 >> 16) as u32;
            if !ctx.is_null() {
                on_command(hwnd, &*ctx, id, code);
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY | WM_NCDESTROY => {
            if msg == WM_NCDESTROY {
                if !ctx.is_null() {
                    drop(Box::from_raw(ctx));
                }
                let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                SETTINGS_HWND.store(0, Ordering::SeqCst);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

#[allow(dead_code)]
fn _unused(_x: HWND) {}

#[allow(clippy::too_many_arguments)] // 裸 Win32 控件创建包装，参数即 CreateWindowExW 实参集。
unsafe fn create_ctl(
    parent: HWND,
    class: &str,
    title: Option<&str>,
    style: WINDOW_STYLE,
    ex: WINDOW_EX_STYLE,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: i32,
    hinst: windows::Win32::Foundation::HINSTANCE,
    hfont: HFONT,
) -> HWND {
    let title_v: Vec<u16> = title.map(wide).unwrap_or_default();
    let class_v = wide(class);
    let hwnd = CreateWindowExW(
        ex,
        PCWSTR(class_v.as_ptr()),
        if title.is_some() {
            PCWSTR(title_v.as_ptr())
        } else {
            PCWSTR::null()
        },
        style,
        x,
        y,
        w,
        h,
        Some(parent),
        Some(HMENU(id as *mut core::ffi::c_void)),
        Some(hinst),
        None,
    )
    .unwrap_or_default();
    if !hwnd.0.is_null() {
        SendMessageW!(hwnd, WM_SETFONT, WPARAM(hfont.0 as usize), LPARAM(1));
    }
    hwnd
}

fn ws(extra: u32) -> WINDOW_STYLE {
    WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0 | extra)
}

unsafe fn build_controls(hwnd: HWND, ctx: &Ctx) {
    let hinst = match GetModuleHandleW(None) {
        Ok(h) => windows::Win32::Foundation::HINSTANCE(h.0),
        Err(_) => return,
    };
    let hfont = HFONT(GetStockObject(DEFAULT_GUI_FONT).0);

    // 分组框（BUTTON + BS_GROUPBOX）。
    create_ctl(hwnd, "BUTTON", Some("外观"), ws(BS_GROUPBOX), WINDOW_EX_STYLE(0), 8, 4, 482, 232, 0, hinst, hfont);
    create_ctl(hwnd, "BUTTON", Some("刷新"), ws(BS_GROUPBOX), WINDOW_EX_STYLE(0), 8, 244, 482, 64, 0, hinst, hfont);
    create_ctl(hwnd, "BUTTON", Some("低电提醒"), ws(BS_GROUPBOX), WINDOW_EX_STYLE(0), 8, 316, 482, 108, 0, hinst, hfont);

    // —— 外观 ——
    create_ctl(hwnd, "STATIC", Some("形状"), ws(0), WINDOW_EX_STYLE(0), 20, 32, 44, 20, 0, hinst, hfont);
    create_ctl(hwnd, "COMBOBOX", None, ws(CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0), WINDOW_EX_STYLE(0), 76, 28, 180, 220, IDC_SHAPE, hinst, hfont);
    create_ctl(hwnd, "STATIC", Some("背景色"), ws(0), WINDOW_EX_STYLE(0), 20, 64, 52, 20, 0, hinst, hfont);
    create_ctl(hwnd, "COMBOBOX", None, ws(CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0), WINDOW_EX_STYLE(0), 76, 60, 110, 200, IDC_BG, hinst, hfont);
    create_ctl(hwnd, "EDIT", None, ws(ES_AUTOHSCROLL as u32), WS_EX_CLIENTEDGE, 196, 60, 70, 22, IDC_BG_HEX, hinst, hfont);
    create_ctl(hwnd, "STATIC", Some("字体"), ws(0), WINDOW_EX_STYLE(0), 20, 96, 44, 20, 0, hinst, hfont);
    create_ctl(hwnd, "COMBOBOX", None, ws(CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0), WINDOW_EX_STYLE(0), 76, 92, 180, 240, IDC_FONT_FAMILY, hinst, hfont);
    create_ctl(hwnd, "STATIC", Some("字体颜色"), ws(0), WINDOW_EX_STYLE(0), 20, 128, 52, 20, 0, hinst, hfont);
    create_ctl(hwnd, "COMBOBOX", None, ws(CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0), WINDOW_EX_STYLE(0), 76, 124, 110, 200, IDC_FONT_COLOR, hinst, hfont);
    create_ctl(hwnd, "EDIT", None, ws(ES_AUTOHSCROLL as u32), WS_EX_CLIENTEDGE, 196, 124, 70, 22, IDC_FONT_HEX, hinst, hfont);
    create_ctl(hwnd, "STATIC", Some("预览"), ws(0), WINDOW_EX_STYLE(0), 20, 160, 44, 20, 0, hinst, hfont);
    create_ctl(hwnd, "STATIC", None, ws(SS_BITMAP), WINDOW_EX_STYLE(0), 76, 152, PREVIEW_PX, PREVIEW_PX, IDC_PREVIEW, hinst, hfont);
    create_ctl(hwnd, "BUTTON", Some("恢复默认"), ws(0), WINDOW_EX_STYLE(0), 290, 152, 100, 26, IDC_RESET_APPEAR, hinst, hfont);

    // —— 刷新 ——
    create_ctl(hwnd, "STATIC", Some("轮询间隔"), ws(0), WINDOW_EX_STYLE(0), 20, 272, 60, 20, 0, hinst, hfont);
    create_ctl(hwnd, "COMBOBOX", None, ws(CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0), WINDOW_EX_STYLE(0), 96, 268, 180, 200, IDC_INTERVAL, hinst, hfont);
    create_ctl(hwnd, "BUTTON", Some("恢复默认"), ws(0), WINDOW_EX_STYLE(0), 290, 266, 100, 26, IDC_RESET_REFRESH, hinst, hfont);

    // —— 低电提醒 ——
    create_ctl(hwnd, "BUTTON", Some("低电时图标闪烁提醒"), ws(BS_AUTOCHECKBOX), WINDOW_EX_STYLE(0), 20, 344, 180, 22, IDC_BLINK, hinst, hfont);
    create_ctl(hwnd, "STATIC", Some("低电阈值"), ws(0), WINDOW_EX_STYLE(0), 20, 380, 60, 20, 0, hinst, hfont);
    create_ctl(hwnd, "msctls_trackbar32", None, ws(TBS_AUTOTICKS), WINDOW_EX_STYLE(0), 96, 376, 180, 32, IDC_SLIDER, hinst, hfont);
    create_ctl(hwnd, "STATIC", Some("20%"), ws(0), WINDOW_EX_STYLE(0), 290, 380, 50, 20, IDC_THRESH_VAL, hinst, hfont);
    create_ctl(hwnd, "BUTTON", Some("恢复默认"), ws(0), WINDOW_EX_STYLE(0), 290, 340, 100, 26, IDC_RESET_LOW, hinst, hfont);

    // —— 底部 ——
    create_ctl(hwnd, "BUTTON", Some("恢复全部默认"), ws(0), WINDOW_EX_STYLE(0), 16, 436, 120, 26, IDC_RESET_ALL, hinst, hfont);
    create_ctl(hwnd, "STATIC", None, ws(0), WINDOW_EX_STYLE(0), 16, 470, 466, 20, IDC_STATUS, hinst, hfont);

    // 组合框内容。
    let shape = GetDlgItem(Some(hwnd), IDC_SHAPE).unwrap_or_default();
    for s in ["圆角（默认）", "直角方块", "透明（无背景）"] {
        add_string(shape, s);
    }
    let bg = GetDlgItem(Some(hwnd), IDC_BG).unwrap_or_default();
    for s in ["深色（默认）", "浅色", "自定义"] {
        add_string(bg, s);
    }
    let fc = GetDlgItem(Some(hwnd), IDC_FONT_COLOR).unwrap_or_default();
    for s in ["自动（按电量档位）", "自定义"] {
        add_string(fc, s);
    }
    let fam = GetDlgItem(Some(hwnd), IDC_FONT_FAMILY).unwrap_or_default();
    for name in enum_fonts() {
        add_string(fam, &name);
    }
    let iv = GetDlgItem(Some(hwnd), IDC_INTERVAL).unwrap_or_default();
    for secs in INTERVALS {
        let label = if secs == DEFAULT_POLL_SECS {
            format!("{secs} 秒（默认）")
        } else {
            format!("{secs} 秒")
        };
        add_string(iv, &label);
    }

    // 滑块：范围 5..=50，刻度间隔 5（M12）。
    let slider = GetDlgItem(Some(hwnd), IDC_SLIDER).unwrap_or_default();
    SendMessageW!(
        slider,
        TBM_SETRANGE,
        WPARAM(1),
        LPARAM(((LOW_THRESHOLD_MAX as isize) << 16) | LOW_THRESHOLD_MIN as isize)
    );
    SendMessageW!(slider, TBM_SETTICFREQ, WPARAM(5), LPARAM(0));

    sync_controls(hwnd, ctx);
}

/// 枚举系统字体（M9）；失败回退常用最小集。
unsafe fn enum_fonts() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let hdc: HDC = GetDC(None);
    if !hdc.is_invalid() {
        let lf = LOGFONTW {
            lfCharSet: DEFAULT_CHARSET,
            ..Default::default()
        };
        let r = EnumFontFamiliesExW(
            hdc,
            &lf,
            Some(font_enum),
            LPARAM(&mut out as *mut Vec<String> as isize),
            0,
        );
        let _ = ReleaseDC(None, hdc);
        if r == 0 {
            out.clear();
        }
    }
    if out.is_empty() {
        out = vec![
            "Segoe UI".into(),
            "Consolas".into(),
            "Arial".into(),
            "Tahoma".into(),
        ];
    }
    out.sort();
    out.dedup();
    out
}

unsafe extern "system" fn font_enum(
    lpelf: *const LOGFONTW,
    _lpntme: *const TEXTMETRICW,
    _ft: u32,
    lparam: LPARAM,
) -> i32 {
    let list = lparam.0 as *mut Vec<String>;
    let name = from_wide_slice(&(*lpelf).lfFaceName);
    // 跳过竖排（@ 前缀）字体。
    if !name.is_empty() && !name.starts_with('@') && !(*list).iter().any(|s| s == &name) {
        (*list).push(name);
    }
    1
}

fn add_string(combo: HWND, text: &str) {
    let v = wide(text);
    unsafe {
        SendMessageW!(combo, CB_ADDSTRING, WPARAM(0), LPARAM(v.as_ptr() as isize));
    }
}

fn combo_sel(combo: HWND) -> i32 {
    unsafe { SendMessageW!(combo, CB_GETCURSEL, WPARAM(0), LPARAM(0)) as i32 }
}

fn combo_set_sel(combo: HWND, idx: i32) {
    unsafe {
        SendMessageW!(combo, CB_SETCURSEL, WPARAM(idx.max(0) as usize), LPARAM(0));
    }
}

fn combo_count(combo: HWND) -> i32 {
    unsafe { SendMessageW!(combo, CB_GETCOUNT, WPARAM(0), LPARAM(0)) as i32 }
}

fn combo_current_text(combo: HWND) -> String {
    unsafe {
        let idx = combo_sel(combo);
        if idx < 0 {
            return String::new();
        }
        let n = SendMessageW!(combo, CB_GETLBTEXTLEN, WPARAM(idx as usize), LPARAM(0)) as i32;
        if n < 0 {
            return String::new();
        }
        let mut buf = vec![0u16; (n + 1) as usize];
        SendMessageW!(
            combo,
            CB_GETLBTEXT,
            WPARAM(idx as usize),
            LPARAM(buf.as_mut_ptr() as isize)
        );
        from_wide_slice(&buf)
    }
}

fn combo_find_exact(combo: HWND, text: &str) -> i32 {
    let v = wide(text);
    unsafe {
        SendMessageW!(combo, CB_FINDSTRINGEXACT, WPARAM(0xFFFF_FFFF), LPARAM(v.as_ptr() as isize))
            as i32
    }
}

unsafe fn edit_text(hwnd: HWND) -> String {
    let mut buf = [0u16; 64];
    let n = GetWindowTextW(hwnd, &mut buf);
    from_wide_slice(&buf[..n.max(0) as usize])
}

fn set_text(ctx: &Ctx, id: i32, text: &str) {
    let v = wide(text);
    unsafe {
        let hwnd = GetDlgItem(Some(settings_hwnd()), id).unwrap_or_default();
        let _ = SetWindowTextW(hwnd, PCWSTR(v.as_ptr()));
    }
    let _ = ctx;
}

fn settings_hwnd() -> HWND {
    HWND(SETTINGS_HWND.load(Ordering::SeqCst) as *mut core::ffi::c_void)
}

fn ctl(hwnd: HWND, id: i32) -> HWND {
    unsafe { GetDlgItem(Some(hwnd), id).unwrap_or_default() }
}

/// 校验外观对比度（M9：<3.0 仅警告不阻断）并刷新预览。
fn refresh_preview_and_hints(hwnd: HWND, ctx: &Ctx) {
    let cfg = ctx.cfg.borrow();
    let rgb = font_rgb(
        IconColor::Default,
        ctx.dark_theme,
        cfg.icon_shape,
        cfg.icon_background,
        cfg.icon_font_color,
    );
    let bg_opt = (cfg.icon_shape != IconShape::Transparent).then_some(cfg.icon_background.rgb());
    let rgba = render_text_bitmap(
        PREVIEW_PX as u32,
        &ctx.preview_text,
        rgb,
        bg_opt,
        cfg.icon_shape,
        &cfg.icon_font_family,
    );

    // 合成到 6 格棋盘底（同时示意深浅两种任务栏），再建 HBITMAP。
    let n = (PREVIEW_PX * PREVIEW_PX) as usize;
    let mut bgra = vec![0u8; n * 4];
    let cell = (PREVIEW_PX / 6).max(1);
    for i in 0..n {
        let (py, px) = ((i / PREVIEW_PX as usize) as i32, (i % PREVIEW_PX as usize) as i32);
        let checker = if ((px / cell) + (py / cell)) % 2 == 0 {
            (0xEEu32, 0xEE, 0xEE)
        } else {
            (0x99u32, 0x99, 0x99)
        };
        let a = rgba[i * 4 + 3] as f32 / 255.0;
        let mix = |fg: u8, bgu: u32| -> u8 {
            ((fg as f32 * a + bgu as f32 * (1.0 - a)) as u16).min(255) as u8
        };
        bgra[i * 4] = mix(rgba[i * 4 + 2], checker.2); // B
        bgra[i * 4 + 1] = mix(rgba[i * 4 + 1], checker.1); // G
        bgra[i * 4 + 2] = mix(rgba[i * 4], checker.0); // R
        bgra[i * 4 + 3] = 255;
    }
    unsafe {
        let preview = ctl(hwnd, IDC_PREVIEW);
        let hbmp = CreateBitmap(
            PREVIEW_PX,
            PREVIEW_PX,
            1,
            32,
            Some(bgra.as_ptr() as *const core::ffi::c_void),
        );
        if !hbmp.is_invalid() {
            let old = SendMessageW!(
                preview,
                STM_SETIMAGE,
                WPARAM(IMAGE_BITMAP.0 as usize),
                LPARAM(hbmp.0 as isize)
            );
            if old != 0 {
                let _ = DeleteObject(HGDIOBJ(old as *mut core::ffi::c_void));
            }
        }
    }

    // 对比度提示（M9：仅警告）。
    let font_c = match cfg.icon_font_color {
        IconFontColor::Fixed(c) => (c.r, c.g, c.b),
        IconFontColor::Auto => rgb,
    };
    let bg_c = match cfg.icon_shape {
        IconShape::Transparent => {
            if ctx.dark_theme {
                (0x20, 0x20, 0x20)
            } else {
                (0xF0, 0xF0, 0xF0)
            }
        }
        _ => cfg.icon_background.rgb(),
    };
    let ratio = contrast_ratio(font_c, bg_c);
    let msg = if contrast_ok(font_c, bg_c) {
        String::new()
    } else {
        format!("⚠ 字色与背景对比度 {ratio:.1} 低于 {CONTRAST_FLOOR}，可能不易读（不阻断使用）")
    };
    drop(cfg);
    set_text(ctx, IDC_STATUS, &msg);
}

/// 程序化同步全部控件 ↔ 工作配置（抑制自身事件）。
fn sync_controls(hwnd: HWND, ctx: &Ctx) {
    ctx.syncing.set(true);
    let cfg = ctx.cfg.borrow();
    unsafe {
        let shape = ctl(hwnd, IDC_SHAPE);
        combo_set_sel(
            shape,
            match cfg.icon_shape {
                IconShape::Rounded => 0,
                IconShape::Square => 1,
                IconShape::Transparent => 2,
            },
        );
        let bg = ctl(hwnd, IDC_BG);
        match cfg.icon_background {
            IconBackground::Dark => combo_set_sel(bg, 0),
            IconBackground::Light => combo_set_sel(bg, 1),
            IconBackground::Custom(c) => {
                combo_set_sel(bg, 2);
                let hex = c.to_hex();
                let v = wide(&hex);
                let _ = SetWindowTextW(ctl(hwnd, IDC_BG_HEX), PCWSTR(v.as_ptr()));
            }
        }
        let fc = ctl(hwnd, IDC_FONT_COLOR);
        match cfg.icon_font_color {
            IconFontColor::Auto => combo_set_sel(fc, 0),
            IconFontColor::Fixed(c) => {
                combo_set_sel(fc, 1);
                let hex = c.to_hex();
                let v = wide(&hex);
                let _ = SetWindowTextW(ctl(hwnd, IDC_FONT_HEX), PCWSTR(v.as_ptr()));
            }
        }
        // 字体：精确找；找不到（自定义/已卸载）补到列表末尾并选中。
        let fam = ctl(hwnd, IDC_FONT_FAMILY);
        let family = cfg.icon_font_family.clone();
        let found = combo_find_exact(fam, &family);
        if found >= 0 {
            combo_set_sel(fam, found);
        } else if !family.is_empty() {
            add_string(fam, &family);
            combo_set_sel(fam, combo_count(fam) - 1);
        }
        // 间隔：取 ≤ 当前值的最大档；低于 15 取 15。
        let iv = ctl(hwnd, IDC_INTERVAL);
        let idx = INTERVALS
            .iter()
            .rposition(|s| *s <= cfg.poll_interval_secs)
            .unwrap_or(0);
        combo_set_sel(iv, idx as i32);

        let blink = ctl(hwnd, IDC_BLINK);
        SendMessageW!(
            blink,
            BM_SETCHECK,
            WPARAM(if cfg.blink_enabled { BST_CHECKED.0 as usize } else { BST_UNCHECKED.0 as usize }),
            LPARAM(0)
        );
        let slider = ctl(hwnd, IDC_SLIDER);
        SendMessageW!(slider, TBM_SETPOS, WPARAM(1), LPARAM(cfg.low_threshold as isize));
        let tv = format!("{}%", cfg.low_threshold);
        let v = wide(&tv);
        let _ = SetWindowTextW(ctl(hwnd, IDC_THRESH_VAL), PCWSTR(v.as_ptr()));
    }
    ctx.syncing.set(false);
    refresh_preview_and_hints(hwnd, ctx);
}

fn on_command(hwnd: HWND, ctx: &Ctx, id: i32, code: u32) {
    if ctx.syncing.get() {
        return;
    }
    let clicked = code == BN_CLICKED;
    let selchange = code == CBN_SELCHANGE;
    let change = code == EN_CHANGE;
    let mut cfg = ctx.cfg.borrow_mut();
    let mut touched = false;
    let mut resync = false;

    match id {
        IDC_SHAPE if selchange => {
            match combo_sel(ctl(hwnd, IDC_SHAPE)) {
                0 => cfg.icon_shape = IconShape::Rounded,
                1 => cfg.icon_shape = IconShape::Square,
                2 => cfg.icon_shape = IconShape::Transparent,
                _ => return,
            }
            touched = true;
        }
        IDC_BG if selchange => {
            match combo_sel(ctl(hwnd, IDC_BG)) {
                0 => cfg.icon_background = IconBackground::Dark,
                1 => cfg.icon_background = IconBackground::Light,
                2 => {
                    let text = unsafe { edit_text(ctl(hwnd, IDC_BG_HEX)) };
                    match RgbColor::parse_hex(&text) {
                        Some(c) => cfg.icon_background = IconBackground::Custom(c),
                        None => {
                            drop(cfg);
                            set_text(ctx, IDC_STATUS, "背景色格式应为 #RRGGBB（如 #1F1F1F）");
                            return;
                        }
                    }
                }
                _ => return,
            }
            touched = true;
        }
        IDC_BG_HEX if change => {
            if let IconBackground::Custom(_) = cfg.icon_background {
                let text = unsafe { edit_text(ctl(hwnd, IDC_BG_HEX)) };
                if let Some(c) = RgbColor::parse_hex(&text) {
                    cfg.icon_background = IconBackground::Custom(c);
                    touched = true;
                }
                // 输入中间态不落地也不提示。
            }
        }
        IDC_FONT_FAMILY if selchange => {
            let name = combo_current_text(ctl(hwnd, IDC_FONT_FAMILY));
            if name.is_empty() {
                return;
            }
            cfg.icon_font_family = name;
            touched = true;
        }
        IDC_FONT_COLOR if selchange => {
            match combo_sel(ctl(hwnd, IDC_FONT_COLOR)) {
                0 => cfg.icon_font_color = IconFontColor::Auto,
                1 => {
                    let text = unsafe { edit_text(ctl(hwnd, IDC_FONT_HEX)) };
                    match RgbColor::parse_hex(&text) {
                        Some(c) => cfg.icon_font_color = IconFontColor::Fixed(c),
                        None => {
                            drop(cfg);
                            set_text(ctx, IDC_STATUS, "字体颜色格式应为 #RRGGBB（如 #FFFFFF）");
                            return;
                        }
                    }
                }
                _ => return,
            }
            touched = true;
        }
        IDC_FONT_HEX if change => {
            if let IconFontColor::Fixed(_) = cfg.icon_font_color {
                let text = unsafe { edit_text(ctl(hwnd, IDC_FONT_HEX)) };
                if let Some(c) = RgbColor::parse_hex(&text) {
                    cfg.icon_font_color = IconFontColor::Fixed(c);
                    touched = true;
                }
            }
        }
        IDC_INTERVAL if selchange => {
            let idx = combo_sel(ctl(hwnd, IDC_INTERVAL));
            if idx >= 0 && (idx as usize) < INTERVALS.len() {
                cfg.poll_interval_secs = INTERVALS[idx as usize];
                touched = true;
            }
        }
        IDC_BLINK if clicked => {
            let checked = unsafe {
                SendMessageW!(ctl(hwnd, IDC_BLINK), BM_GETCHECK, WPARAM(0), LPARAM(0))
            };
            cfg.blink_enabled = checked == BST_CHECKED.0 as isize;
            touched = true;
        }
        IDC_RESET_APPEAR if clicked => {
            cfg.icon_shape = IconShape::Rounded;
            cfg.icon_background = IconBackground::Dark;
            cfg.icon_font_color = IconFontColor::Auto;
            cfg.icon_font_family = "Segoe UI".to_string();
            touched = true;
            resync = true;
        }
        IDC_RESET_REFRESH if clicked => {
            cfg.poll_interval_secs = DEFAULT_POLL_SECS;
            touched = true;
            resync = true;
        }
        IDC_RESET_LOW if clicked => {
            cfg.blink_enabled = true;
            cfg.low_threshold = DEFAULT_LOW_THRESHOLD;
            touched = true;
            resync = true;
        }
        IDC_RESET_ALL if clicked => {
            *cfg = Config::default();
            touched = true;
            resync = true;
        }
        _ => {}
    }

    if touched {
        ctx.apply(&cfg);
        drop(cfg);
        if resync {
            sync_controls(hwnd, ctx);
        } else {
            refresh_preview_and_hints(hwnd, ctx);
        }
    }
}
