//! 设置窗口（v0.3.0 产品方案 M9-M13；v0.3.3 #3/#4）：裸 Win32 单页非模态窗口。
//!
//! 形态：无模式、单实例（重复打开只前置既有窗口）、改动即时生效（无确定/取消）、
//! 关闭即销毁。GUI 改动直接写入共享配置（AppCore 同一个 `Arc<RwLock<Config>>`）
//! 并持久化 TOML；TOML 侧改动经既有 mtime 热重载进入共享配置——双向一致。
//!
//! v0.3.3：
//! - #3 所有颜色入口（背景色 / 字体颜色 / 分档色点）点击「选色…」呼出系统
//!   取色对话框（ChooseColorW，展开完整色轮 + 自定义色），HEX 手输框保留、
//!   双向同步（D-41）；
//! - #4 字体颜色分档：启用后隐藏单一字体颜色入口；N 档 = N-1 个阈值数字框
//!   （该数字及以下的电量归上一档）+ N 个色点，色点点击走同一取色对话框；
//!   阈值校验 1–99 严格递增（D-42）。
//!
//! 三组控件 + 每组「恢复默认」+ 全局「恢复全部默认」：
//! - 外观：形状（圆角/直角/透明）、背景色（深/浅/自定义 #RRGGBB）、字体（系统
//!   字体枚举）、字体颜色（auto / 自定义 #RRGGBB / 分档）、实时预览；对比度 <3.0
//!   仅警告不阻断（M9）；
//! - 刷新：轮询间隔 15/30/60/120/300 秒（M10）；
//! - 低电提醒：闪烁开关 + 阈值滑块 5..=50 步进 5（M11/M12）。

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use volttray_app::autostart;
use volttray_app::config::{
    contrast_ok, contrast_ratio, font_tier_for_percent, normalize_font_tiers, Config,
    IconBackground, IconFontColor, IconShape, RgbColor, CONTRAST_FLOOR, DEFAULT_FONT_TIER_COLORS,
    DEFAULT_FONT_TIER_THRESHOLDS, DEFAULT_LOW_THRESHOLD, DEFAULT_POLL_SECS, FONT_TIER_MAX_TIERS,
    FONT_TIER_MIN_TIERS, FONT_TIER_THRESHOLD_MAX, FONT_TIER_THRESHOLD_MIN, LOW_THRESHOLD_MAX,
    LOW_THRESHOLD_MIN, ICON_BG_SCALE_MAX, ICON_BG_SCALE_MIN, ICON_TEXT_SCALE_MAX,
    ICON_TEXT_SCALE_MIN,
};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateDIBSection, DeleteObject, EnumFontFamiliesExW, GetDC, GetStockObject, ReleaseDC, DEFAULT_CHARSET, DEFAULT_GUI_FONT, HBRUSH, HDC, HFONT, HGDIOBJ, LOGFONTW,
    TEXTMETRICW, HBITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, BST_CHECKED, BST_UNCHECKED, ICC_BAR_CLASSES, INITCOMMONCONTROLSEX,
    TBM_SETPOS, TBM_SETRANGE, TBM_SETTICFREQ, TBS_AUTOTICKS,
};
// comdlg32（ChooseColorW 等）在 windows crate 的 Controls::Dialogs 子模块。
use windows::Win32::UI::Controls::Dialogs::{ChooseColorW, CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetDlgItem, GetWindowLongPtrW, GetWindowTextW,
    IsWindow, RegisterClassW, SendMessageW, SetForegroundWindow, SetWindowLongPtrW, SetWindowTextW,
    ShowWindow, BM_GETCHECK, BM_SETCHECK, BN_CLICKED, CBN_SELCHANGE, CBS_DROPDOWNLIST,
    CB_ADDSTRING, CB_FINDSTRINGEXACT, CB_GETCOUNT, CB_GETCURSEL, CB_GETLBTEXT, CB_GETLBTEXTLEN,
    CB_SETCURSEL, CREATESTRUCTW, CW_USEDEFAULT, EN_CHANGE, ES_AUTOHSCROLL, GWLP_USERDATA, HCURSOR,
    HICON, HMENU, IMAGE_BITMAP, STM_SETIMAGE, SW_HIDE, SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_ACTIVATE, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_HSCROLL, WM_NCDESTROY, WM_SETFONT,
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

// windows crate 0.62 未导出的控件常量（本地定义）：
// TBM_GETPOS = WM_USER+0；SS_BITMAP = 0x0E。
const TBM_GETPOS: u32 = 0x0400;
const SS_BITMAP: u32 = 14;
/// SS_NOTIFY = 0x0100（STATIC 点击通知 STN_CLICKED；windows crate 未导出）。
const SS_NOTIFY: u32 = 0x0100;
/// ES_NUMBER = 0x2000（EDIT 仅数字；windows crate 未导出）。
const ES_NUMBER: u32 = 0x2000;

/// BS_GROUPBOX（分组框样式；windows crate 以 i32 导出，这里只用作样式位）。
const BS_GROUPBOX: u32 = 7;
/// BS_AUTOCHECKBOX。
const BS_AUTOCHECKBOX: u32 = 3;

/// 设置窗口单实例句柄（0 = 未打开）。
static SETTINGS_HWND: AtomicIsize = AtomicIsize::new(0);

/// ChooseColorW 自定义颜色槽（进程级持久；comdlg32 要求可写指针）。
static CUSTOM_COLORS: Mutex<[COLORREF; 16]> = Mutex::new([COLORREF(0); 16]);

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
/// 文本大小滑块（v0.3.4 #4，D-48）。
const IDC_TEXT_SLIDER: i32 = 2036;
const IDC_TEXT_VAL: i32 = 2037;
/// 背景大小滑块（v0.3.4 #4，D-48）。
const IDC_BG_SLIDER: i32 = 2038;
const IDC_BG_VAL: i32 = 2039;
/// 背景色「选色…」按钮（#3 色轮入口）。
const IDC_BG_PICK: i32 = 2017;
/// 字体颜色「选色…」按钮（#3 色轮入口）。
const IDC_FONT_PICK: i32 = 2018;
/// 「字体颜色分档」复选框（#4）。
const IDC_TIERED: i32 = 2019;
/// 分档数量下拉（2..=5 档）。
const IDC_TIER_COUNT: i32 = 2020;
/// 分档阈值数字框（最多 4 个 = 5 档 - 1）。
const IDC_TIER_TH0: i32 = 2021;
/// 分档色点（最多 5 个）。
const IDC_TIER_DOT0: i32 = 2031;

/// 预览位图边长（像素）。
const PREVIEW_PX: i32 = 48;
/// 分档色点边长（像素）。
const DOT_PX: i32 = 26;

/// 刷新间隔档位（v0.3.4 #2 / D-45）：过短档位（15/30/120 秒）与零耗电理念
/// 相悖且无收益（电量变化约 1%–2%/小时），收敛为 60/300/600/900/1800。
const INTERVALS: [u64; 5] = [60, 300, 600, 900, 1800];

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
        let title = wide("LogiVoltTray 设置");
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
            724,
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

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
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
            // 后的控件状态）。仅激活时同步。P3-2：WA_CLICKACTIVE（鼠标点击激活）
            // 低位为 1、WA_ACTIVE 为 1、WA_INACTIVE 为 0——「非 0」覆盖点击激活。
            if wparam.0 & 0xFFFF != 0 && !ctx.is_null() {
                let ctx = &*ctx;
                if let Ok(guard) = ctx.arc.read() {
                    *ctx.cfg.borrow_mut() = guard.clone();
                    sync_controls(hwnd, ctx);
                }
            }
            LRESULT(0)
        }
        WM_HSCROLL => {
            // 滑块（发给父窗口；lparam = 滑块 HWND）：低电阈值 + 文本/背景大小。
            if !ctx.is_null() {
                let ctx = &*ctx;
                let lp = lparam.0 as *mut core::ffi::c_void;
                if !ctx.syncing.get() {
                    let slider = GetDlgItem(Some(hwnd), IDC_SLIDER).unwrap_or_default();
                    let ts_ctl = GetDlgItem(Some(hwnd), IDC_TEXT_SLIDER).unwrap_or_default();
                    let bs_ctl = GetDlgItem(Some(hwnd), IDC_BG_SLIDER).unwrap_or_default();
                    if slider.0 == lp {
                        let pos = SendMessageW!(slider, TBM_GETPOS, WPARAM(0), LPARAM(0)) as i32;
                        let snapped = ((pos as f64 / 5.0).round() * 5.0) as i64;
                        let v =
                            snapped.clamp(LOW_THRESHOLD_MIN as i64, LOW_THRESHOLD_MAX as i64) as u8;
                        let mut cfg = ctx.cfg.borrow_mut();
                        if cfg.low_threshold != v {
                            cfg.low_threshold = v;
                            let text = format!("{}%", v);
                            set_text(ctx, IDC_THRESH_VAL, &text);
                            ctx.apply(&cfg);
                        }
                    } else if ts_ctl.0 == lp {
                        // v0.3.4 #4（D-48）：文本大小 60..=140%，即时生效并持久化。
                        let pos = SendMessageW!(ts_ctl, TBM_GETPOS, WPARAM(0), LPARAM(0)) as i32;
                        let v = pos
                            .clamp(ICON_TEXT_SCALE_MIN as i32, ICON_TEXT_SCALE_MAX as i32)
                            as u8;
                        let mut cfg = ctx.cfg.borrow_mut();
                        if cfg.icon_text_scale != v {
                            cfg.icon_text_scale = v;
                            let text = format!("{v}%");
                            set_text(ctx, IDC_TEXT_VAL, &text);
                            ctx.apply(&cfg);
                        }
                    } else if bs_ctl.0 == lp {
                        // v0.3.4 #4（D-48）：背景大小 60..=100%（>100% 会溢出内切块）。
                        let pos = SendMessageW!(bs_ctl, TBM_GETPOS, WPARAM(0), LPARAM(0)) as i32;
                        let v = pos.clamp(ICON_BG_SCALE_MIN as i32, ICON_BG_SCALE_MAX as i32) as u8;
                        let mut cfg = ctx.cfg.borrow_mut();
                        if cfg.icon_bg_scale != v {
                            cfg.icon_bg_scale = v;
                            let text = format!("{v}%");
                            set_text(ctx, IDC_BG_VAL, &text);
                            ctx.apply(&cfg);
                        }
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
    create_ctl(
        hwnd,
        "BUTTON",
        Some("外观"),
        ws(BS_GROUPBOX),
        WINDOW_EX_STYLE(0),
        8,
        4,
        482,
        316,
        0,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "BUTTON",
        Some("刷新"),
        ws(BS_GROUPBOX),
        WINDOW_EX_STYLE(0),
        8,
        328,
        482,
        64,
        0,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "BUTTON",
        Some("低电提醒"),
        ws(BS_GROUPBOX),
        WINDOW_EX_STYLE(0),
        8,
        400,
        482,
        108,
        0,
        hinst,
        hfont,
    );
    // v0.3.4 #4（D-48）：图标大小分组——文本 60..=140%、背景 60..=100%。
    create_ctl(
        hwnd,
        "BUTTON",
        Some("图标大小"),
        ws(BS_GROUPBOX),
        WINDOW_EX_STYLE(0),
        8,
        514,
        482,
        96,
        0,
        hinst,
        hfont,
    );

    // —— 外观 ——
    create_ctl(
        hwnd,
        "STATIC",
        Some("形状"),
        ws(0),
        WINDOW_EX_STYLE(0),
        20,
        32,
        44,
        20,
        0,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "COMBOBOX",
        None,
        ws(CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0),
        WINDOW_EX_STYLE(0),
        76,
        28,
        180,
        220,
        IDC_SHAPE,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "STATIC",
        Some("背景色"),
        ws(0),
        WINDOW_EX_STYLE(0),
        20,
        64,
        52,
        20,
        0,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "COMBOBOX",
        None,
        ws(CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0),
        WINDOW_EX_STYLE(0),
        76,
        60,
        110,
        200,
        IDC_BG,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "EDIT",
        None,
        ws(ES_AUTOHSCROLL as u32),
        WS_EX_CLIENTEDGE,
        196,
        60,
        70,
        22,
        IDC_BG_HEX,
        hinst,
        hfont,
    );
    // #3：色轮入口（系统取色对话框，展开完整调色板）。
    create_ctl(
        hwnd,
        "BUTTON",
        Some("选色…"),
        ws(0),
        WINDOW_EX_STYLE(0),
        274,
        58,
        54,
        24,
        IDC_BG_PICK,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "STATIC",
        Some("字体"),
        ws(0),
        WINDOW_EX_STYLE(0),
        20,
        96,
        44,
        20,
        0,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "COMBOBOX",
        None,
        ws(CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0),
        WINDOW_EX_STYLE(0),
        76,
        92,
        180,
        240,
        IDC_FONT_FAMILY,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "STATIC",
        Some("字体颜色"),
        ws(0),
        WINDOW_EX_STYLE(0),
        20,
        128,
        52,
        20,
        0,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "COMBOBOX",
        None,
        ws(CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0),
        WINDOW_EX_STYLE(0),
        76,
        124,
        110,
        200,
        IDC_FONT_COLOR,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "EDIT",
        None,
        ws(ES_AUTOHSCROLL as u32),
        WS_EX_CLIENTEDGE,
        196,
        124,
        70,
        22,
        IDC_FONT_HEX,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "BUTTON",
        Some("选色…"),
        ws(0),
        WINDOW_EX_STYLE(0),
        274,
        122,
        54,
        24,
        IDC_FONT_PICK,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "STATIC",
        Some("预览"),
        ws(0),
        WINDOW_EX_STYLE(0),
        20,
        160,
        44,
        20,
        0,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "STATIC",
        None,
        ws(SS_BITMAP),
        WINDOW_EX_STYLE(0),
        76,
        152,
        PREVIEW_PX,
        PREVIEW_PX,
        IDC_PREVIEW,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "BUTTON",
        Some("恢复默认"),
        ws(0),
        WINDOW_EX_STYLE(0),
        290,
        152,
        100,
        26,
        IDC_RESET_APPEAR,
        hinst,
        hfont,
    );

    // —— 字体颜色分档（#4）——
    create_ctl(
        hwnd,
        "BUTTON",
        Some("字体颜色分档"),
        ws(BS_AUTOCHECKBOX),
        WINDOW_EX_STYLE(0),
        20,
        212,
        132,
        22,
        IDC_TIERED,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "STATIC",
        Some("档数"),
        ws(0),
        WINDOW_EX_STYLE(0),
        162,
        214,
        32,
        20,
        0,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "COMBOBOX",
        None,
        ws(CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0),
        WINDOW_EX_STYLE(0),
        198,
        210,
        64,
        140,
        IDC_TIER_COUNT,
        hinst,
        hfont,
    );
    // 阈值数字框（N 档 = N-1 个；该数字及以下电量归上一档）。
    for i in 0..4 {
        create_ctl(
            hwnd,
            "EDIT",
            None,
            ws(ES_NUMBER),
            WS_EX_CLIENTEDGE,
            62 + i * 44,
            242,
            38,
            22,
            IDC_TIER_TH0 + i,
            hinst,
            hfont,
        );
    }
    // 分档色点（N 个；点击呼出取色对话框）。
    for i in 0..5 {
        create_ctl(
            hwnd,
            "STATIC",
            None,
            ws(SS_BITMAP | SS_NOTIFY),
            WINDOW_EX_STYLE(0),
            62 + i * 44,
            268,
            DOT_PX,
            DOT_PX,
            IDC_TIER_DOT0 + i,
            hinst,
            hfont,
        );
    }
    create_ctl(
        hwnd,
        "STATIC",
        Some("阈值"),
        ws(0),
        WINDOW_EX_STYLE(0),
        20,
        246,
        36,
        20,
        0,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "STATIC",
        Some("阈值需 1–99 严格递增；数字及以下的电量归上一档（20/50 → 0-20、21-50、51-100）"),
        ws(0),
        WINDOW_EX_STYLE(0),
        20,
        294,
        452,
        20,
        0,
        hinst,
        hfont,
    );

    // —— 刷新 ——
    create_ctl(
        hwnd,
        "STATIC",
        Some("轮询间隔"),
        ws(0),
        WINDOW_EX_STYLE(0),
        20,
        356,
        60,
        20,
        0,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "COMBOBOX",
        None,
        ws(CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0),
        WINDOW_EX_STYLE(0),
        96,
        352,
        180,
        200,
        IDC_INTERVAL,
        hinst,
        hfont,
    );
    // D-33：轮询间隔耗电提示——每次 HID++ 查询都是一次射频交互，
    // 更短的间隔 = 更频繁唤醒鼠标无线链路 = 更快耗电池。
    create_ctl(
        hwnd,
        "STATIC",
        Some("提示：更短的间隔会增加鼠标耗电；电量变化很慢，默认档已足够及时"),
        ws(0),
        WINDOW_EX_STYLE(0),
        96,
        380,
        320,
        20,
        0,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "BUTTON",
        Some("恢复默认"),
        ws(0),
        WINDOW_EX_STYLE(0),
        290,
        350,
        100,
        26,
        IDC_RESET_REFRESH,
        hinst,
        hfont,
    );

    // —— 低电提醒 ——
    create_ctl(
        hwnd,
        "BUTTON",
        Some("低电时图标闪烁提醒"),
        ws(BS_AUTOCHECKBOX),
        WINDOW_EX_STYLE(0),
        20,
        428,
        180,
        22,
        IDC_BLINK,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "STATIC",
        Some("低电阈值"),
        ws(0),
        WINDOW_EX_STYLE(0),
        20,
        464,
        60,
        20,
        0,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "msctls_trackbar32",
        None,
        ws(TBS_AUTOTICKS),
        WINDOW_EX_STYLE(0),
        96,
        460,
        180,
        32,
        IDC_SLIDER,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "STATIC",
        Some("20%"),
        ws(0),
        WINDOW_EX_STYLE(0),
        290,
        464,
        50,
        20,
        IDC_THRESH_VAL,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "BUTTON",
        Some("恢复默认"),
        ws(0),
        WINDOW_EX_STYLE(0),
        290,
        424,
        100,
        26,
        IDC_RESET_LOW,
        hinst,
        hfont,
    );

    // —— 图标大小（v0.3.4 #4，D-48）——
    create_ctl(
        hwnd,
        "STATIC",
        Some("文本大小"),
        ws(0),
        WINDOW_EX_STYLE(0),
        20,
        534,
        60,
        20,
        0,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "msctls_trackbar32",
        None,
        ws(TBS_AUTOTICKS),
        WINDOW_EX_STYLE(0),
        96,
        530,
        180,
        32,
        IDC_TEXT_SLIDER,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "STATIC",
        Some("100%"),
        ws(0),
        WINDOW_EX_STYLE(0),
        290,
        534,
        44,
        20,
        IDC_TEXT_VAL,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "STATIC",
        Some("背景大小"),
        ws(0),
        WINDOW_EX_STYLE(0),
        20,
        570,
        64,
        20,
        0,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "msctls_trackbar32",
        None,
        ws(TBS_AUTOTICKS),
        WINDOW_EX_STYLE(0),
        96,
        566,
        180,
        32,
        IDC_BG_SLIDER,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "STATIC",
        Some("100%"),
        ws(0),
        WINDOW_EX_STYLE(0),
        290,
        570,
        44,
        20,
        IDC_BG_VAL,
        hinst,
        hfont,
    );

    // —— 底部 ——
    create_ctl(
        hwnd,
        "BUTTON",
        Some("恢复全部默认"),
        ws(0),
        WINDOW_EX_STYLE(0),
        16,
        618,
        120,
        26,
        IDC_RESET_ALL,
        hinst,
        hfont,
    );
    create_ctl(
        hwnd,
        "STATIC",
        None,
        ws(0),
        WINDOW_EX_STYLE(0),
        16,
        652,
        466,
        20,
        IDC_STATUS,
        hinst,
        hfont,
    );

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
    let tc = GetDlgItem(Some(hwnd), IDC_TIER_COUNT).unwrap_or_default();
    for k in FONT_TIER_MIN_TIERS..=FONT_TIER_MAX_TIERS {
        add_string(tc, &format!("{k} 档"));
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
    // v0.3.4 #4（D-48）：文本 60..=140%、背景 60..=100%，刻度间隔 10%。
    let ts = GetDlgItem(Some(hwnd), IDC_TEXT_SLIDER).unwrap_or_default();
    SendMessageW!(
        ts,
        TBM_SETRANGE,
        WPARAM(1),
        LPARAM(((ICON_TEXT_SCALE_MAX as isize) << 16) | ICON_TEXT_SCALE_MIN as isize)
    );
    SendMessageW!(ts, TBM_SETTICFREQ, WPARAM(10), LPARAM(0));
    let bs = GetDlgItem(Some(hwnd), IDC_BG_SLIDER).unwrap_or_default();
    SendMessageW!(
        bs,
        TBM_SETRANGE,
        WPARAM(1),
        LPARAM(((ICON_BG_SCALE_MAX as isize) << 16) | ICON_BG_SCALE_MIN as isize)
    );
    SendMessageW!(bs, TBM_SETTICFREQ, WPARAM(10), LPARAM(0));

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
        SendMessageW!(
            combo,
            CB_FINDSTRINGEXACT,
            WPARAM(0xFFFF_FFFF),
            LPARAM(v.as_ptr() as isize)
        ) as i32
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

/// 显示/隐藏控件（分档模式下隐藏单一字体颜色入口、多余阈值框与色点）。
unsafe fn show_ctl(hwnd: HWND, id: i32, show: bool) {
    let _ = ShowWindow(ctl(hwnd, id), if show { SW_SHOW } else { SW_HIDE });
}

/// 呼出系统取色对话框（#3：CC_FULLOPEN 展开完整色轮 + 自定义色区）。
/// 返回 None = 用户取消。模态期间不持有任何配置借用（WM_ACTIVATE 同步会重入）。
unsafe fn pick_color(owner: HWND, initial: RgbColor) -> Option<RgbColor> {
    let mut custom = CUSTOM_COLORS.lock().unwrap_or_else(|e| e.into_inner());
    let mut cc = CHOOSECOLORW {
        lStructSize: std::mem::size_of::<CHOOSECOLORW>() as u32,
        hwndOwner: owner,
        hInstance: HWND::default(),
        rgbResult: COLORREF(
            (initial.b as u32) | ((initial.g as u32) << 8) | ((initial.r as u32) << 16),
        ),
        lpCustColors: custom.as_mut_ptr(),
        Flags: CC_RGBINIT | CC_FULLOPEN,
        lCustData: LPARAM(0),
        lpfnHook: None,
        lpTemplateName: PCWSTR::null(),
    };
    if ChooseColorW(&mut cc).as_bool() {
        let c = cc.rgbResult.0;
        Some(RgbColor {
            r: ((c >> 16) & 0xFF) as u8,
            g: ((c >> 8) & 0xFF) as u8,
            b: (c & 0xFF) as u8,
        })
    } else {
        None
    }
}

/// 生成 DOT_PX² 的分档色点位图（BGRA，1px 抗锯齿边缘的实心圆点）。
fn tier_dot_bgra(c: RgbColor) -> Vec<u8> {
    let size = DOT_PX;
    let center = (size as f32 - 1.0) / 2.0;
    let radius = (size as f32 / 2.0) - 1.5; // 留 1.5px 边距
    let mut v = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let d = (dx * dx + dy * dy).sqrt();
            // 1px 线性抗锯齿过渡。
            let cov = (radius - d).clamp(-1.0, 1.0) * 0.5 + 0.5;
            let i = ((y * size + x) * 4) as usize;
            v[i] = c.b;
            v[i + 1] = c.g;
            v[i + 2] = c.r;
            v[i + 3] = (cov * 255.0).round() as u8;
        }
    }
    v
}

/// v0.3.4 #1（同类修复）：把直 alpha BGRA 打成**预乘 32bpp DIB**（HBITMAP）。
/// SS_BITMAP 控件只对 32bpp DIB 按 alpha 混合；CreateBitmap 建的是 DDB，
/// alpha 被忽略——抗锯齿圆点会画成实心色块（与托盘 CreateIcon 的
/// DDB alpha 根因同类）。预乘公式 (v*a+127)/255 与图标渲染路径一致。
unsafe fn bgra_dib_premultiplied(bgra: &[u8], w: i32, h: i32) -> HBITMAP {
    let bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        bmiColors: [windows::Win32::Graphics::Gdi::RGBQUAD::default()],
    };
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let Ok(hbmp) = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
        return windows::Win32::Graphics::Gdi::HBITMAP::default();
    };
    if !bits.is_null() {
        let dst = std::slice::from_raw_parts_mut(bits as *mut u8, bgra.len());
        for (o, px) in dst.chunks_exact_mut(4).zip(bgra.chunks_exact(4)) {
            let a = px[3] as u32;
            o[0] = ((px[0] as u32 * a + 127) / 255) as u8;
            o[1] = ((px[1] as u32 * a + 127) / 255) as u8;
            o[2] = ((px[2] as u32 * a + 127) / 255) as u8;
            o[3] = px[3];
        }
    }
    hbmp
}

/// 把分档颜色画到色点控件（SS_BITMAP）。
unsafe fn draw_dot(hwnd: HWND, id: i32, color: RgbColor) {
    let bgra = tier_dot_bgra(color);
    let dot = ctl(hwnd, id);
    let hbmp = bgra_dib_premultiplied(&bgra, DOT_PX, DOT_PX);
    if !hbmp.is_invalid() {
        let old = SendMessageW!(
            dot,
            STM_SETIMAGE,
            WPARAM(IMAGE_BITMAP.0 as usize),
            LPARAM(hbmp.0 as isize)
        );
        if old != 0 {
            let _ = DeleteObject(HGDIOBJ(old as *mut core::ffi::c_void));
        }
    }
}

/// 校验外观对比度（M9：<3.0 仅警告不阻断）并刷新预览。
fn refresh_preview_and_hints(hwnd: HWND, ctx: &Ctx) {
    let cfg = ctx.cfg.borrow();
    // #4：分档启用时预览用当前电量对应的分档色（预览文本非数字则回退自动色）。
    let tiered = cfg.icon_font_tiers_enabled;
    let tiers: Option<&[RgbColor]> = if tiered {
        Some(cfg.icon_font_tier_colors.as_slice())
    } else {
        None
    };
    // P3-3 修复：预览用色与托盘实显对齐——"满"=100 参与分档；
    // 分档启用且非数字文本（?/--）用 Gray（托盘语义），而非 Default。
    let percent: Option<u8> = if ctx.preview_text == "满" {
        Some(100)
    } else {
        ctx.preview_text.parse().ok()
    };
    let level = if let (true, Some(p)) = (tiered, percent) {
        IconColor::Tier(font_tier_for_percent(&cfg.icon_font_tier_thresholds, p) as u8)
    } else if tiered && percent.is_none() {
        IconColor::Gray
    } else {
        IconColor::Default
    };
    let rgb = font_rgb(
        level,
        ctx.dark_theme,
        cfg.icon_shape,
        cfg.icon_background,
        cfg.icon_font_color,
        tiers,
    );
    let bg_opt = (cfg.icon_shape != IconShape::Transparent).then_some(cfg.icon_background.rgb());
    let rgba = render_text_bitmap(
        PREVIEW_PX as u32,
        &ctx.preview_text,
        rgb,
        bg_opt,
        cfg.icon_shape,
        &cfg.icon_font_family,
        cfg.icon_text_scale,
        cfg.icon_bg_scale,
    );

    // 合成到 6 格棋盘底（同时示意深浅两种任务栏），再建 HBITMAP。
    let n = (PREVIEW_PX * PREVIEW_PX) as usize;
    let mut bgra = vec![0u8; n * 4];
    let cell = (PREVIEW_PX / 6).max(1);
    for i in 0..n {
        let (py, px) = (
            (i / PREVIEW_PX as usize) as i32,
            (i % PREVIEW_PX as usize) as i32,
        );
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
        // v0.3.4 #1：预览换预乘 DIB（此处 alpha 恒 255，预乘为恒等；
        // 与托盘 HICON 同路径，保证「预览 = 托盘实显」的颜色语义一致）。
        let hbmp = bgra_dib_premultiplied(&bgra, PREVIEW_PX, PREVIEW_PX);
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

    // 对比度提示（M9：仅警告）。分档模式取当前档颜色（无数字回退最高档）。
    let font_c = if tiered {
        let t = match percent {
            Some(p) => font_tier_for_percent(&cfg.icon_font_tier_thresholds, p),
            None => cfg.icon_font_tier_colors.len().saturating_sub(1),
        };
        cfg.icon_font_tier_colors
            .get(t)
            .map(|c| (c.r, c.g, c.b))
            .unwrap_or(rgb)
    } else {
        match cfg.icon_font_color {
            IconFontColor::Fixed(c) => (c.r, c.g, c.b),
            IconFontColor::Auto => rgb,
        }
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
        // 间隔：取 ≤ 当前值的最大档；低于 60 取 60（D-45）。
        let iv = ctl(hwnd, IDC_INTERVAL);
        let idx = INTERVALS
            .iter()
            .rposition(|s| *s <= cfg.poll_interval_secs)
            .unwrap_or(0);
        combo_set_sel(iv, idx as i32);

        // —— 字体颜色分档（#4）——
        let tiered = cfg.icon_font_tiers_enabled;
        SendMessageW!(
            ctl(hwnd, IDC_TIERED),
            BM_SETCHECK,
            WPARAM(if tiered {
                BST_CHECKED.0 as usize
            } else {
                BST_UNCHECKED.0 as usize
            }),
            LPARAM(0)
        );
        // 启用分档后隐藏单一字体颜色入口（用户原话：按钮右侧就不需要单一颜色选择）。
        show_ctl(hwnd, IDC_FONT_COLOR, !tiered);
        show_ctl(hwnd, IDC_FONT_HEX, !tiered);
        show_ctl(hwnd, IDC_FONT_PICK, !tiered);
        let n = cfg
            .icon_font_tier_colors
            .len()
            .clamp(FONT_TIER_MIN_TIERS, FONT_TIER_MAX_TIERS);
        combo_set_sel(ctl(hwnd, IDC_TIER_COUNT), (n - FONT_TIER_MIN_TIERS) as i32);
        let ths = cfg.icon_font_tier_thresholds.clone();
        for i in 0..4usize {
            let e = ctl(hwnd, IDC_TIER_TH0 + i as i32);
            let _ = ShowWindow(e, if i < n - 1 { SW_SHOW } else { SW_HIDE });
            if let Some(t) = ths.get(i) {
                let v = wide(&t.to_string());
                let _ = SetWindowTextW(e, PCWSTR(v.as_ptr()));
            }
        }
        let cols = cfg.icon_font_tier_colors.clone();
        for i in 0..5usize {
            let d = IDC_TIER_DOT0 + i as i32;
            let _ = ShowWindow(ctl(hwnd, d), if i < n { SW_SHOW } else { SW_HIDE });
            if let Some(c) = cols.get(i) {
                draw_dot(hwnd, d, *c);
            }
        }

        let blink = ctl(hwnd, IDC_BLINK);
        SendMessageW!(
            blink,
            BM_SETCHECK,
            WPARAM(if cfg.blink_enabled {
                BST_CHECKED.0 as usize
            } else {
                BST_UNCHECKED.0 as usize
            }),
            LPARAM(0)
        );
        let slider = ctl(hwnd, IDC_SLIDER);
        SendMessageW!(
            slider,
            TBM_SETPOS,
            WPARAM(1),
            LPARAM(cfg.low_threshold as isize)
        );
        let tv = format!("{}%", cfg.low_threshold);
        let v = wide(&tv);
        let _ = SetWindowTextW(ctl(hwnd, IDC_THRESH_VAL), PCWSTR(v.as_ptr()));
        // v0.3.4 #4（D-48）：文本/背景大小滑块回填。
        SendMessageW!(
            ctl(hwnd, IDC_TEXT_SLIDER),
            TBM_SETPOS,
            WPARAM(1),
            LPARAM(cfg.icon_text_scale as isize)
        );
        let tvs = format!("{}%", cfg.icon_text_scale);
        let vv = wide(&tvs);
        let _ = SetWindowTextW(ctl(hwnd, IDC_TEXT_VAL), PCWSTR(vv.as_ptr()));
        SendMessageW!(
            ctl(hwnd, IDC_BG_SLIDER),
            TBM_SETPOS,
            WPARAM(1),
            LPARAM(cfg.icon_bg_scale as isize)
        );
        let bvs = format!("{}%", cfg.icon_bg_scale);
        let bv = wide(&bvs);
        let _ = SetWindowTextW(ctl(hwnd, IDC_BG_VAL), PCWSTR(bv.as_ptr()));
    }
    ctx.syncing.set(false);
    refresh_preview_and_hints(hwnd, ctx);
}

/// 「选色…」按钮与分档色点点击：先取初始色（短借用）→ 呼出模态取色 → 回写。
unsafe fn handle_color_pick(hwnd: HWND, ctx: &Ctx, id: i32) {
    let initial = {
        let cfg = ctx.cfg.borrow();
        match id {
            IDC_BG_PICK => match cfg.icon_background {
                IconBackground::Custom(c) => c,
                IconBackground::Dark => RgbColor {
                    r: 0x1F,
                    g: 0x1F,
                    b: 0x1F,
                },
                IconBackground::Light => RgbColor {
                    r: 0xF2,
                    g: 0xF2,
                    b: 0xF2,
                },
            },
            IDC_FONT_PICK => match cfg.icon_font_color {
                IconFontColor::Fixed(c) => c,
                IconFontColor::Auto => RgbColor {
                    r: 0xFF,
                    g: 0xFF,
                    b: 0xFF,
                },
            },
            _ => {
                let i = (id - IDC_TIER_DOT0) as usize;
                cfg.icon_font_tier_colors
                    .get(i)
                    .copied()
                    .unwrap_or(RgbColor {
                        r: 0x80,
                        g: 0x80,
                        b: 0x80,
                    })
            }
        }
    };
    let Some(picked) = pick_color(hwnd, initial) else {
        return; // 用户取消
    };
    {
        let mut cfg = ctx.cfg.borrow_mut();
        match id {
            IDC_BG_PICK => {
                cfg.icon_background = IconBackground::Custom(picked);
                // 「自定义」档被选中，HEX 框与下拉在下一次 sync 对齐。
            }
            IDC_FONT_PICK => {
                cfg.icon_font_color = IconFontColor::Fixed(picked);
            }
            _ => {
                let i = (id - IDC_TIER_DOT0) as usize;
                if i < cfg.icon_font_tier_colors.len() {
                    cfg.icon_font_tier_colors[i] = picked;
                }
            }
        }
        ctx.apply(&cfg);
    }
    sync_controls(hwnd, ctx);
}

fn on_command(hwnd: HWND, ctx: &Ctx, id: i32, code: u32) {
    if ctx.syncing.get() {
        return;
    }
    let clicked = code == BN_CLICKED;
    // #3：取色按钮 / 分档色点（STN_CLICKED 与 BN_CLICKED 同为 0）。
    if clicked
        && (id == IDC_BG_PICK
            || id == IDC_FONT_PICK
            || (IDC_TIER_DOT0..IDC_TIER_DOT0 + 5).contains(&id))
    {
        unsafe { handle_color_pick(hwnd, ctx, id) };
        return;
    }
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
        // #4：分档开关——关闭恢复单一字体颜色选择（控件显隐在 sync_controls）。
        IDC_TIERED if clicked => {
            let checked =
                unsafe { SendMessageW!(ctl(hwnd, IDC_TIERED), BM_GETCHECK, WPARAM(0), LPARAM(0)) };
            cfg.icon_font_tiers_enabled = checked == BST_CHECKED.0 as isize;
            touched = true;
            resync = true;
        }
        // #4：分档数量——调整阈值数组（增长时在上个阈值 +30、封顶 99），
        // 再过一遍 normalize（钳制 + 修补严格递增 + 颜色补齐/截断）。
        IDC_TIER_COUNT if selchange => {
            let idx = combo_sel(ctl(hwnd, IDC_TIER_COUNT));
            if idx < 0 {
                return;
            }
            let n = FONT_TIER_MIN_TIERS + idx as usize;
            let mut ths = cfg.icon_font_tier_thresholds.clone();
            while ths.len() + 1 > n {
                ths.pop();
            }
            // P3-2 修复：+30 递增会让顶档只覆盖 100%（如 20/50/80/99）。
            // 改为在 [上一档+1, 99] 区间均分剩余档位，保证顶档覆盖到 100%。
            while ths.len() + 1 < n {
                let lo = ths.last().map(|t| *t as i32).unwrap_or(0);
                let slots = n - ths.len() - 1; // 除顶档外还需生成的档数
                let next = if slots > 0 {
                    (99 - lo) / (slots as i32 + 1) + lo
                } else {
                    (99 + lo) / 2
                };
                let next = next.clamp(lo + 1, FONT_TIER_THRESHOLD_MAX as i32) as u8;
                if next as i32 <= lo {
                    break; // 无严格递增空间；normalize 会兜底。
                }
                ths.push(next);
            }
            let (t2, c2, _) = normalize_font_tiers(ths, cfg.icon_font_tier_colors.clone());
            cfg.icon_font_tier_thresholds = t2;
            cfg.icon_font_tier_colors = c2;
            touched = true;
            resync = true;
        }
        // #4：阈值数字框——1–99、严格递增，非法即时提示且不落地。
        id if change && (IDC_TIER_TH0..IDC_TIER_TH0 + 4).contains(&id) => {
            let i = (id - IDC_TIER_TH0) as usize;
            let text = unsafe { edit_text(ctl(hwnd, id)) };
            if text.is_empty() {
                set_text(ctx, IDC_STATUS, ""); // 输入中间态
                return;
            }
            let v: u8 = match text.parse::<i64>() {
                Ok(v)
                    if (FONT_TIER_THRESHOLD_MIN as i64..=FONT_TIER_THRESHOLD_MAX as i64)
                        .contains(&v) =>
                {
                    v as u8
                }
                _ => {
                    set_text(
                        ctx,
                        IDC_STATUS,
                        "分档阈值需为 1–99 的整数（数字及以下的电量归上一档）",
                    );
                    return;
                }
            };
            if i >= cfg.icon_font_tier_thresholds.len() {
                return;
            }
            let mut ths = cfg.icon_font_tier_thresholds.clone();
            ths[i] = v;
            if !ths.windows(2).all(|w| w[0] < w[1]) {
                set_text(
                    ctx,
                    IDC_STATUS,
                    "分档阈值必须严格递增（如 20、50：0-20 一档、21-50 一档、51-100 一档）",
                );
                return;
            }
            cfg.icon_font_tier_thresholds = ths;
            set_text(ctx, IDC_STATUS, "");
            touched = true;
        }
        IDC_INTERVAL if selchange => {
            let idx = combo_sel(ctl(hwnd, IDC_INTERVAL));
            if idx >= 0 && (idx as usize) < INTERVALS.len() {
                cfg.poll_interval_secs = INTERVALS[idx as usize];
                touched = true;
            }
        }
        IDC_BLINK if clicked => {
            let checked =
                unsafe { SendMessageW!(ctl(hwnd, IDC_BLINK), BM_GETCHECK, WPARAM(0), LPARAM(0)) };
            cfg.blink_enabled = checked == BST_CHECKED.0 as isize;
            touched = true;
        }
        IDC_RESET_APPEAR if clicked => {
            cfg.icon_shape = IconShape::Rounded;
            cfg.icon_background = IconBackground::Dark;
            cfg.icon_font_color = IconFontColor::Auto;
            cfg.icon_font_family = "Segoe UI".to_string();
            // #4：分档键一并恢复默认。
            cfg.icon_font_tiers_enabled = false;
            cfg.icon_font_tier_thresholds = DEFAULT_FONT_TIER_THRESHOLDS.to_vec();
            cfg.icon_font_tier_colors = DEFAULT_FONT_TIER_COLORS.to_vec();
            // v0.3.4 #4：文本/背景大小一并恢复默认。
            cfg.icon_text_scale = 100;
            cfg.icon_bg_scale = 100;
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
            // P2-3：注册表自启态与默认值对齐，避免 TOML 与 HKCU Run 键分叉。
            let exe =
                std::env::current_exe().unwrap_or_else(|_| PathBuf::from("volttray-tray.exe"));
            let _ = autostart::sync(cfg.autostart, &exe);
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
