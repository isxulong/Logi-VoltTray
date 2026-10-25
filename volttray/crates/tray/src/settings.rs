//! 设置窗口（v0.3.5 全面现代化重构）：裸 Win32 单页非模态窗口。
//!
//! 形态：无模式、单实例（重复打开只前置既有窗口）、改动即时生效（无确定/取消）、
//! 关闭即销毁。GUI 改动直接写入共享配置（AppCore 同一个 `Arc<RwLock<Config>>`）
//! 并持久化 TOML；TOML 侧改动经既有 mtime 热重载进入共享配置——双向一致。
//!
//! v0.3.5（#7696143042746518551 设计规范 v1.0 逐条落地）：
//! - #1 字节序（D-52）：所有 Win32 COLORREF（0x00BBGGRR）↔ `RgbColor`（#RRGGBB）
//!   转换统一走 `config::rgb_to_colorref` / `config::colorref_to_rgb`，全窗口
//!   不存在第二份字节序实现；取色对话框 → 配置 → 渲染链路字节序一致。
//! - #2 程序图标（D-53）：EXE 嵌入 assets/volttray.ico（16/24/32/48/64/128/256），
//!   运行时 `LoadIconW(MAKEINTRESOURCE(1))` 设置窗口大/小图标 + 窗口类图标。
//! - #3 滚轮方向（D-54）：滑块与数字框统一「上滚增加、下滚减少」（规范裁决 5），
//!   经 GWLP_WNDPROC 子类化注入（windows crate 0.62 未导出 SetWindowSubclass，
//!   改用 SetWindowLongPtrW + CallWindowProcW）；数字框同时支持 ↑/↓ 键。
//! - #5/#6 字体颜色合一（D-56）：删除独立「字体颜色分档」复选框，下拉
//!   [自动（按电量档位）, 自定义] 即分档开关；「自动」展开分档卡片（档数/
//!   阈值/色点/提示），「自定义」收起并显示单一选色 swatch，空间经 relayout 回收。
//! - #7 文字不截断：分区标题/标签按 9pt、提示语按 8pt 实测文本宽度，
//!   最长提示语反推窗口最小宽度（规范裁决 3），任何文案不再截断/裁切。
//! - #8 图标大小范围（D-57）：文本 40–160%、背景覆盖度 20–100%（100% = 满出血），
//!   常量统一来自 config（ICON_*_SCALE_*）。
//! - #9 现代化（D-58）：分区标题 + 8px 网格 + 分色 token（浅/深）、按钮
//!   owner-draw（悬停/按下态）、深色标题栏（DWMWA_USE_IMMERSIVE_DARK_MODE）、
//!   DPI 感知（GetDpiForWindow + AdjustWindowRectExForDpi）、分档色点 hover
//!   tooltip（档位区间 + HEX）。原生控件（下拉/滑块/复选框）保持系统主题，
//!   不引入 WebView/大型框架。
//!
//! v0.3.6（红蓝对抗审查收敛：3 个 P2 + 同批 P3）：
//! - F3 焦点态（D-59）：Theme 增加 accent token（浅 #0078D4 / 深 #60CDFF），
//!   owner-draw 按钮处理 ODS_FOCUS 画 2px 强调色内描边，键盘 Tab 走查可见。
//! - F2 阈值非法反馈（D-60）：非法阈值框 1px 错误色描边 + 分档卡内右侧红色
//!   提示（不占底部状态行，避免被对比度提示覆写）；非法值不落地。
//! - P3（D-62）：轮询档单一来源 POLL_TIERS_SECS；自动→自定义→自动往返保留
//!   已选自定义色（last_custom_font）；滑块行补最小—最大标注；分档说明示例
//!   按当前档数/阈值实时拼写（tier_hint_dynamic）。
//!
//! 低电提醒：闪烁开关 + 阈值滑块 5..=50 步进 5；透明形状下低电闪烁作用于
//! 文本透明度脉冲（icon.rs / blink.rs，D-55），本窗口仅含开关与阈值。

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::icon::{cache_from_config, IconColor};
use volttray_app::autostart;
// E23（报告口径 A-R4② 第 1 步）：纯逻辑文案拼写已抽离到 app crate
//（跨平台可测）；本文件只保留 Win32 侧的控件装配与调用。
use volttray_app::config::{
    colorref_to_rgb, font_tier_for_percent, normalize_font_tiers, rgb_to_colorref, Config,
    IconBackground, IconFontColor, IconShape, RgbColor, DEFAULT_LOW_THRESHOLD, FONT_TIER_MAX_TIERS,
    FONT_TIER_MIN_TIERS, FONT_TIER_THRESHOLD_MAX, FONT_TIER_THRESHOLD_MIN, ICON_BG_SCALE_MAX,
    ICON_BG_SCALE_MIN, ICON_TEXT_SCALE_DEFAULT, ICON_TEXT_SCALE_MAX, ICON_TEXT_SCALE_MIN,
    LOW_THRESHOLD_MAX, LOW_THRESHOLD_MIN, POLL_TIERS_SECS,
};
// E23（报告口径 A-R4② 第 2 步）：阈值校验 / 档位映射 / applied 日志行
// 拼写同样抽离到 app crate（跨平台受测）；本文件只剩 Win32 控件装配。
use volttray_app::settings_logic::{
    parse_tier_threshold, retier_thresholds, thresholds_strictly_increasing,
    TIER_THRESHOLD_ERR_NOT_INCREASING,
};
use volttray_app::settings_text::{
    fine_tune_gate_text, lab_hint_text, poll_tier_label, settings_applied_note, tier_hint_dynamic,
    LAB_HINT_TEXT,
};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    GetLastError, COLORREF, ERROR_CLASS_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, LRESULT, POINT,
    RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWINDOWATTRIBUTE};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateDIBSection, CreateFontIndirectW, CreatePen, CreateSolidBrush, DeleteObject,
    DrawTextW, EndPaint, EnumFontFamiliesExW, FillRect, FrameRect, GetDC, GetStockObject,
    GetTextExtentPoint32W, InvalidateRect, Rectangle, ReleaseDC, RoundRect, ScreenToClient,
    SelectObject, SetBkMode, SetTextColor, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DEFAULT_CHARSET,
    DEFAULT_GUI_FONT, DIB_RGB_COLORS, DT_CENTER, DT_SINGLELINE, DT_VCENTER, HBITMAP, HBRUSH, HDC,
    HFONT, HGDIOBJ, LOGFONTW, NULL_BRUSH, PAINTSTRUCT, PS_SOLID, TEXTMETRICW, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::Dialogs::{ChooseColorW, CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW};
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, BST_CHECKED, BST_UNCHECKED, DRAWITEMSTRUCT, ICC_BAR_CLASSES,
    ICC_WIN95_CLASSES, INITCOMMONCONTROLSEX, ODS_SELECTED, TBM_SETPOS, TBM_SETRANGE,
    TBM_SETTICFREQ, TBS_AUTOTICKS, TTF_IDISHWND, TTF_SUBCLASS, TTM_ADDTOOLW, TTM_UPDATETIPTEXTW,
    TTTOOLINFOW,
};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    TrackMouseEvent, TME_HOVER, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetCursorPos,
    GetDlgCtrlID, GetDlgItem, GetParent, GetWindowLongPtrW, GetWindowRect, GetWindowTextW,
    IsWindow, LoadCursorW, LoadIconW, MessageBoxW, MoveWindow, RegisterClassW, SendMessageW,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow, BM_GETCHECK,
    BM_SETCHECK, BN_CLICKED, CBN_SELCHANGE, CBS_DROPDOWNLIST, CB_ADDSTRING, CB_FINDSTRINGEXACT,
    CB_GETCOUNT, CB_GETCURSEL, CB_GETLBTEXT, CB_GETLBTEXTLEN, CB_SETCURSEL, CREATESTRUCTW,
    CW_USEDEFAULT, EN_CHANGE, GWLP_USERDATA, GWLP_WNDPROC, HCURSOR, HICON, HMENU, IDC_ARROW,
    IMAGE_BITMAP, MB_ICONWARNING, MB_OK, STM_SETIMAGE, SWP_NOMOVE, SWP_NOZORDER, SW_HIDE, SW_SHOW,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_ACTIVATE, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_CTLCOLORBTN,
    WM_CTLCOLOREDIT, WM_CTLCOLORSTATIC, WM_DESTROY, WM_DRAWITEM, WM_ERASEBKGND, WM_HSCROLL,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCDESTROY, WM_PAINT, WM_SETFONT, WM_SETICON, WNDCLASSW,
    WNDCLASS_STYLES, WNDPROC, WS_CAPTION, WS_CHILD, WS_MINIMIZEBOX, WS_OVERLAPPED, WS_POPUP,
    WS_SYSMENU, WS_VISIBLE, WS_VSCROLL,
};

/// SendMessageW 包装（Option 参数形态见 0.62 绑定；返回 LRESULT 内层值）。
macro_rules! SendMessageW {
    ($hwnd:expr, $msg:expr, $wp:expr, $lp:expr) => {
        SendMessageW($hwnd, $msg, Some($wp), Some($lp)).0
    };
}

// —— windows crate 0.62 未导出 / 类型不便利的控件常量（本地定义）——
/// TBM_GETPOS = WM_USER+0；TBM_GETRANGEMIN/MAX = WM_USER+1/2。
const TBM_GETPOS: u32 = 0x0400;
const TBM_GETRANGEMIN: u32 = 0x0401;
const TBM_GETRANGEMAX: u32 = 0x0402;
/// TB_THUMBPOSITION（转发给父窗口的 WM_HSCROLL 通知码）。
const TB_THUMBPOSITION: u32 = 4;
/// SS_BITMAP = 0x0E。
const SS_BITMAP: u32 = 14;
/// SS_NOTIFY = 0x0100（STATIC 点击通知 STN_CLICKED）。
const SS_NOTIFY: u32 = 0x0100;
/// ES_NUMBER = 0x2000（EDIT 仅数字）。
const ES_NUMBER: u32 = 0x2000;
/// BS_AUTOCHECKBOX = 0x03。
const BS_AUTOCHECKBOX: u32 = 3;
/// BS_OWNERDRAW = 0x0B（BUTTON 样式常量 crate 以 i32 导出，本地取位值）。
const BS_OWNERDRAW_STYLE: u32 = 0x000B;
/// WS_EX_CLIENTEDGE = 0x0200。
const WS_EX_CLIENTEDGE_RAW: u32 = 0x0200;
/// WM_MOUSEHOVER / WM_MOUSELEAVE（未在 0.62 导出；与 WinUser.h 一致）。
const WM_MOUSEHOVER: u32 = 0x02A1;
const WM_MOUSELEAVE: u32 = 0x02A3;
/// WM_KEYDOWN。
const WM_KEYDOWN: u32 = 0x0100;
const VK_UP: usize = 0x26;
const VK_DOWN: usize = 0x28;
/// WM_CTLCOLORLISTBOX（下拉列表部分）。
const WM_CTLCOLORLISTBOX: u32 = 0x0134;
/// WM_SETICON 的 wParam：ICON_SMALL=0 / ICON_BIG=1。
const ICON_SMALL_ID: usize = 0;
const ICON_BIG_ID: usize = 1;
/// DWMWA_USE_IMMERSIVE_DARK_MODE（Win10 1809+；DWMWINDOWATTRIBUTE 为 i32）。
const DWMWA_USE_IMMERSIVE_DARK_MODE: i32 = 20;
/// tooltip 弹出层样式 TTS_ALWAYSTIP(0x01) | TTS_NOPREFIX(0x02)。
const TTS_FLAGS_RAW: u32 = 0x01 | 0x02;
/// ODS_FOCUS = 0x0010（DRAWITEM 焦点态；v0.3.6 焦点描边用，本地定义）。
const ODS_FOCUS_RAW: u32 = 0x0010;
/// 设置窗口固定样式（无缩放边框：布局由代码精确控制）。
const SETTINGS_STYLE: u32 = WS_OVERLAPPED.0 | WS_CAPTION.0 | WS_SYSMENU.0 | WS_MINIMIZEBOX.0;

/// 设置窗口单实例句柄（0 = 未打开）。
static SETTINGS_HWND: AtomicIsize = AtomicIsize::new(0);

/// ChooseColorW 自定义颜色槽（进程级持久；comdlg32 要求可写指针）。
static CUSTOM_COLORS: Mutex<[COLORREF; 16]> = Mutex::new([COLORREF(0); 16]);

/// v0.3.22（D-88）：四项试验功能的进程级基线快照——进程内首次打开设置
/// 窗口时记录一次，用于 hint 行「已修改，重启后生效」动态判定（窗口
/// 反复开关不重置；进程重启才复位）。
static LAB_BASELINE: Mutex<Option<[bool; 4]>> = Mutex::new(None);

// —— 控件 ID（v0.3.5：删除 HEX 框/分档勾选/分组恢复按钮，新增 swatch 与行内重置）——
const IDC_SHAPE: i32 = 2001;
const IDC_BG: i32 = 2002;
const IDC_FONT_FAMILY: i32 = 2004;
const IDC_FONT_COLOR: i32 = 2005;
const IDC_RESET_LOW: i32 = 2010;
const IDC_RESET_ALL: i32 = 2011;
const IDC_BLINK: i32 = 2012;
const IDC_SLIDER: i32 = 2013;
const IDC_THRESH_VAL: i32 = 2014;
// v0.3.18（U-6）：IDC_STATUS(2015) 随对比度警告条一并删除。
const IDC_INTERVAL: i32 = 2016;
/// 背景色「选色…」按钮（色轮入口）。
const IDC_BG_PICK: i32 = 2017;
/// 字体颜色「选色…」按钮（色轮入口）。
const IDC_FONT_PICK: i32 = 2018;
/// 分档数量下拉（2..=5 档）。
const IDC_TIER_COUNT: i32 = 2020;
/// 分档阈值数字框（最多 4 个 = 5 档 - 1）。
const IDC_TIER_TH0: i32 = 2021;
/// 分档色点（最多 5 个）。
const IDC_TIER_DOT0: i32 = 2031;
/// 文本大小滑块（D-48；v0.3.5 范围 40–160%）。
const IDC_TEXT_SLIDER: i32 = 2036;
const IDC_TEXT_VAL: i32 = 2037;
/// 背景覆盖度滑块（D-57；v0.3.5 范围 20–100%，100% = 满出血）。
const IDC_BG_SLIDER: i32 = 2038;
const IDC_BG_VAL: i32 = 2039;
/// 行内「重置」按钮（文本大小 / 背景覆盖度）。
const IDC_TEXT_RESET: i32 = 2040;
const IDC_BG_RESET: i32 = 2041;
/// 背景色 swatch（40×24 色块，显示当前背景色）。
const IDC_BG_SWATCH: i32 = 2042;
/// 字体颜色 swatch（「自定义」模式显示；「自动」模式隐藏）。
const IDC_FONT_SWATCH: i32 = 2043;
/// 分档卡片内提示语。
const IDC_TIER_HINT: i32 = 2044;
/// 背景覆盖度说明。
const IDC_COVER_HINT: i32 = 2045;
/// 放大预览（v0.3.18 U-3：144 逻辑像素画布，不再标注倍数）。
const IDC_PREVIEW_BIG: i32 = 2046;
/// 轮询间隔说明提示。
const IDC_INTERVAL_HINT: i32 = 2047;
/// 分档卡内右侧红色错误提示（v0.3.6 F2：阈值非法反馈不占状态行）。
const IDC_TIER_ERR: i32 = 2048;
/// 分档卡片内标签（档数/阈值/颜色）。
const IDC_LBL_TIER_COUNT: i32 = 2050;
const IDC_LBL_TIER_TH: i32 = 2051;
const IDC_LBL_TIER_COLOR: i32 = 2052;
/// 需要随分档卡收起而移动的标签/分区标题。
const IDC_LBL_TEXT: i32 = 2060;
const IDC_LBL_BGCOVER: i32 = 2061;
const IDC_SEC_REFRESH: i32 = 2063;
const IDC_LBL_INTERVAL: i32 = 2064;
const IDC_SEC_LOW: i32 = 2065;
const IDC_LBL_LOW: i32 = 2066;
/// 滑块行最小—最大范围标注（v0.3.6 规范偏差修正）。
const IDC_TEXT_RANGE: i32 = 2067;
const IDC_BG_RANGE: i32 = 2068;
const IDC_LOW_RANGE: i32 = 2069;
/// R-8（v0.3.17 真机终审）：放大预览的独立标签；v0.3.18（U-3）不再标注
/// 倍数（用户只看布局，实际大小以任务栏为准），标签文案固定「预览」。
const IDC_LBL_PREVIEW_BIG: i32 = 2070;
/// 实验室 hint 行（v0.3.22 D-87/D-88：终版口径 + 动态「已修改」判定）。
const IDC_EXPERIMENTAL_HINT: i32 = 2071;
/// v0.3.22（D-87）：「精细化设置」闸门复选框（实验室入口；复用
/// IDC_BLINK 的 BS_AUTOCHECKBOX 链路；计数常驻动态显示，收起态可见）。
const IDC_FINE_TUNE_GATE: i32 = 2072;
/// 闸门旁小字（收起仅隐藏选项，已开启的功能继续生效——收起≠关闭）。
const IDC_GATE_NOTE: i32 = 2073;
/// 实验室分组框（BS_GROUPBOX；标题「实验室（试验功能）」，D-87）。
const IDC_LAB_GROUP: i32 = 2074;
/// 实验室四项复选框（白话标签，不复现 experimental_* 键名，D-87）。
const IDC_LAB_PERSIST: i32 = 2075;
const IDC_LAB_STATS: i32 = 2076;
const IDC_LAB_POLLING: i32 = 2077;
const IDC_LAB_BT: i32 = 2078;
/// 四项各自的一行说明。
const IDC_LAB_PERSIST_DESC: i32 = 2079;
const IDC_LAB_STATS_DESC: i32 = 2080;
const IDC_LAB_POLLING_DESC: i32 = 2081;
const IDC_LAB_BT_DESC: i32 = 2082;
/// 实验室内「全部恢复默认（开启）」链接式按钮（一键对齐，D-86；
/// 复用 IDC_RESET_ALL 的 owner-draw 模式，仅作用于四键）。
const IDC_LAB_RESET: i32 = 2083;
/// 「立即重启」非模态按钮（D-88：复用 current_exe 启动路径 spawn 新
/// 进程 + 优雅退出；排程代码零改动，非热生效）。
const IDC_LAB_RESTART: i32 = 2084;
/// BS_GROUPBOX = 0x07（BUTTON 分组框样式；crate 未导出该常量，本地取位值）。
const BS_GROUPBOX_STYLE: u32 = 0x0007;

/// v0.3.18（U-3/U-5）：放大预览画布边长（逻辑像素）——4×(192) 略大，
/// 收敛到 144（96dpi 下恰为 3×）；画布与源位图（托盘档位尺寸）推得
/// 整数放大倍率，不写死。源位图边长即 tray_size，不再有独立预览尺寸。
const PREVIEW_BIG_PX: i32 = 144;
/// 分档色点边长（规范裁决 2：16×16 圆点）。
const DOT_PX: i32 = 16;

// —— 布局常量（逻辑像素，96dpi 基准；渲染时按窗口 DPI 缩放）——
/// 客户区最小逻辑宽（实际按最长提示语实测拓宽）。
const L_CLIENT_W: i32 = 500;
/// 分档卡片逻辑高（3 行 + 提示行；规范 96 的微调：避免 8pt 提示贴边）。
const L_CARD_H: i32 = 104;
/// 展开态客户区逻辑高（v0.3.17 R-9：提示行 32→44，+12；
/// v0.3.18 U-6：对比度警告状态行删除，842→816 = 「恢复全部默认」
/// 按钮底边 796 + 20 下边距；相位二第 4 项：底部追加实验性功能
/// 标注行，816→854 = 标注底边 834 + 20 下边距）。
const L_EXPANDED_H: i32 = 854;
/// 收起分档卡后回收的逻辑高度（卡片 104 + 间距 8）。
const TIER_DELTA: i32 = L_CARD_H + 8;
/// 收起态客户区逻辑高。
const L_COLLAPSED_H: i32 = L_EXPANDED_H - TIER_DELTA;
/// v0.3.22（D-87）：实验室分组展开时追加的客户区逻辑高——窗口高度沿用
/// 分档卡同一 relayout 路径（L_EXPANDED_H / L_COLLAPSED_H + 本增量），
/// 不引入新窗口机制。实验室分组 852..1136 + 20 下边距 = 1156 - 854。
const EXP_GATE_DELTA: i32 = 302;

/// 分档卡提示语宽度实测样本（裁决 3：按 5 档最长示例反推窗口最小宽；
/// 实际显示文本由 `tier_hint_dynamic` 按当前档数/阈值实时拼写，v0.3.6）。
const TIER_HINT_SAMPLE: &str =
    "阈值 1–99 严格递增；电量 ≤ 数值归入对应档（10/30/50/70 → 0–10 / 11–30 / 31–50 / 51–70 / 71–100）";
/// 背景覆盖度说明（裁决 4：100% = 满出血）。
const COVER_HINT_TEXT: &str = "背景覆盖度 100% = 满出血（覆盖整个内切区域）";
/// 轮询间隔说明提示（D-66：如实说明轮询电耗可忽略，不再暗示「间隔越短越耗电」）。
/// 单行放不下 → IDC_INTERVAL_HINT 为三行高静态控件（默认 SS_LEFT 自动换行，
/// 宽度随客户区 = cw-104；v0.3.12 修复：此前单行高 14px 在窗口右缘截断；
/// v0.3.17 R-9 追加保活扫描说明后升为三行）。
const INTERVAL_HINT_TEXT: &str = "轮询本身几乎不消耗鼠标电量（每次查询仅约 2 个射频数据包）；间隔只需保证显示足够新鲜。休眠中的设备不会被主动唤醒查询。设备活动期间另有独立的连接保活扫描，其频率与间隔设置无关。";
/// 闸门旁小字（D-87：收起仅隐藏选项，已开启的功能继续生效——收起≠关闭）。
const GATE_NOTE_TEXT: &str = "收起仅隐藏选项，已开启的功能继续生效";
/// 实验室分组框标题（D-87：组名统一「实验室（试验功能）」）。
const LAB_GROUP_TITLE: &str = "实验室（试验功能）";
/// 四项是否有未保存改动（D-88）：与进程内首次打开设置窗口时的基线比对。
fn lab_dirty(ctx: &Ctx) -> bool {
    let base = LAB_BASELINE.lock().ok().and_then(|g| *g);
    let Some(base) = base else { return false };
    let cfg = ctx.cfg.borrow();
    [
        cfg.experimental_persist_last_known,
        cfg.experimental_stats,
        cfg.experimental_per_device_polling,
        cfg.experimental_bluetooth_wired,
    ] != base
}

/// 实验室四项白话标签与一行说明（D-87：不复现 experimental_* 键名）。
/// 文案与实现口径一致：persist=记住上次电量、stats=耗电统计可导出、
/// per_device_polling=充电时 15 秒快档、bluetooth_wired=蓝牙/有线直连。
static LAB_ITEMS: [(i32, &str, &str); 4] = [
    (
        IDC_LAB_PERSIST,
        "重启后记住电量",
        "记住设备最后一次电量：设备断开后仍显示上次读数，重启软件也不清空。",
    ),
    (
        IDC_LAB_STATS,
        "耗电统计与导出",
        "在后台记录设备的耗电与充电历史，可在托盘菜单导出为文件查看（历史文件按大小轮转保留）。",
    ),
    (
        IDC_LAB_POLLING,
        "充电时加快刷新（15 秒）",
        "设备充电时按 15 秒间隔刷新电量，比常规间隔更及时；其余情况仍按常规间隔。",
    ),
    (
        IDC_LAB_BT,
        "蓝牙与有线设备直连读取",
        "直接发现并读取蓝牙或有线直连的设备（不经接收器）；关闭后只识别接收器连接的设备。",
    ),
];

/// 展开态下方控件坐标表：(id, x, y, w, h)（逻辑像素）。
/// 收起分档卡时整组上移 TIER_DELTA；提示语宽度在 relayout 中按实测宽覆盖。
const BELOW: [(i32, i32, i32, i32, i32); 39] = [
    (IDC_LBL_TEXT, 16, 278, 64, 20),
    (IDC_TEXT_SLIDER, 88, 274, 196, 26),
    (IDC_TEXT_VAL, 292, 278, 44, 20),
    (IDC_TEXT_RESET, 344, 274, 48, 24),
    (IDC_TEXT_RANGE, 400, 280, 84, 14),
    (IDC_LBL_BGCOVER, 16, 310, 72, 20),
    (IDC_BG_SLIDER, 88, 306, 196, 26),
    (IDC_BG_VAL, 292, 310, 44, 20),
    (IDC_BG_RESET, 344, 306, 48, 24),
    (IDC_BG_RANGE, 400, 312, 84, 14),
    (IDC_COVER_HINT, 88, 338, 396, 14),
    // v0.3.18（U-2/U-3）：小预览删除，大预览收敛为 144×144 并左移到
    // x=88（原小预览位）；标签单行「预览」，与位图垂直居中（中心 434）。
    (IDC_LBL_PREVIEW_BIG, 16, 424, 68, 20),
    (IDC_PREVIEW_BIG, 88, 362, PREVIEW_BIG_PX, PREVIEW_BIG_PX),
    (IDC_SEC_REFRESH, 16, 566, 80, 18),
    (IDC_LBL_INTERVAL, 16, 592, 64, 20),
    (IDC_INTERVAL, 88, 588, 160, 220),
    // R-9：提示升三行（32→44），其下整组 +12 联动；窗口展开高 +12。
    (IDC_INTERVAL_HINT, 88, 620, 396, 44),
    (IDC_SEC_LOW, 16, 676, 96, 18),
    (IDC_BLINK, 16, 702, 240, 22),
    (IDC_LBL_LOW, 16, 732, 64, 20),
    (IDC_SLIDER, 88, 728, 196, 26),
    (IDC_THRESH_VAL, 292, 732, 44, 20),
    (IDC_RESET_LOW, 344, 728, 48, 24),
    (IDC_LOW_RANGE, 400, 734, 84, 14),
    (IDC_RESET_ALL, 16, 768, 120, 28),
    // v0.3.22（D-87）：精细化设置闸门（常驻可见；计数动态显示）+ 闸门小字。
    (IDC_FINE_TUNE_GATE, 16, 802, 320, 20),
    (IDC_GATE_NOTE, 16, 824, 468, 14),
    // v0.3.22（D-87/D-88）：实验室分组框与四项（收起即隐藏；闸门只管可见性
    // 不管使能——收起≠关闭）。hint 移入组内，随组一并显隐。
    (IDC_LAB_GROUP, 16, 852, 468, 284),
    (IDC_LAB_PERSIST, 28, 878, 420, 20),
    (IDC_LAB_PERSIST_DESC, 44, 898, 424, 14),
    (IDC_LAB_STATS, 28, 920, 420, 20),
    (IDC_LAB_STATS_DESC, 44, 940, 424, 14),
    (IDC_LAB_POLLING, 28, 962, 420, 20),
    (IDC_LAB_POLLING_DESC, 44, 982, 424, 14),
    (IDC_LAB_BT, 28, 1004, 420, 20),
    (IDC_LAB_BT_DESC, 44, 1024, 424, 14),
    (IDC_EXPERIMENTAL_HINT, 28, 1048, 440, 44),
    (IDC_LAB_RESET, 28, 1100, 168, 24),
    (IDC_LAB_RESTART, 204, 1100, 104, 24),
];

/// 色彩 token（规范裁决 6：浅/深两套；存储为 COLORREF 0x00BBGGRR，
/// 一律经 `rgb_to_colorref` 从 #RRGGBB 语义构造，杜绝第二份字节序实现）。
#[derive(Clone, Copy)]
struct Theme {
    window: u32,
    card: u32,
    border: u32,
    text: u32,
    secondary: u32,
    error: u32,
    hover: u32,
    pressed: u32,
    /// 强调色（v0.3.6 F3：焦点 2px 内描边；浅 #0078D4 / 深 #60CDFF，规范裁决 6）。
    accent: u32,
}

/// #RRGGBB 语义 → COLORREF（0x00BBGGRR）。全文件唯一的颜色→Win32 通道（D-52）。
fn tc(r: u8, g: u8, b: u8) -> u32 {
    rgb_to_colorref(RgbColor { r, g, b })
}

fn theme(dark: bool) -> Theme {
    if dark {
        Theme {
            window: tc(0x20, 0x20, 0x20),
            card: tc(0x2B, 0x2B, 0x2B),
            border: tc(0x3F, 0x3F, 0x3F),
            text: tc(0xFF, 0xFF, 0xFF),
            secondary: tc(0xC8, 0xC8, 0xC8),
            error: tc(0xFF, 0x99, 0xA4),
            hover: tc(0x38, 0x38, 0x38),
            pressed: tc(0x48, 0x48, 0x48),
            accent: tc(0x60, 0xCD, 0xFF),
        }
    } else {
        Theme {
            window: tc(0xFF, 0xFF, 0xFF),
            card: tc(0xF5, 0xF5, 0xF5),
            border: tc(0xE0, 0xE0, 0xE0),
            text: tc(0x1B, 0x1B, 0x1B),
            secondary: tc(0x61, 0x61, 0x61),
            error: tc(0xC4, 0x2B, 0x1C),
            hover: tc(0xF0, 0xF0, 0xF0),
            pressed: tc(0xE0, 0xE0, 0xE0),
            accent: tc(0x00, 0x78, 0xD4),
        }
    }
}

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
    /// v0.3.18（U-5）：托盘图标位图边长（当前 DPI 档）——预览单一渲染源
    /// 的渲染尺寸，打开窗口时由主事件循环传入。
    tray_size: u32,
    /// 程序化同步控件时抑制自身事件。
    syncing: Cell<bool>,
    /// 预览文本（如当前电量数字）。
    preview_text: String,
    /// 色彩 token（浅/深）。
    theme: Theme,
    /// 窗口 DPI（WM_CREATE 时取）。
    dpi: Cell<u32>,
    /// 客户区逻辑宽（按实测提示语宽度决定，≥ L_CLIENT_W）。
    cw: Cell<i32>,
    /// 分档卡是否展开（驱动 relayout）。
    expanded: Cell<bool>,
    /// v0.3.22（D-87）：实验室闸门是否展开（驱动 relayout；收起仅隐藏
    /// 选项，不改四项使能——收起≠关闭）。
    gate_expanded: Cell<bool>,
    /// 阈值数字框非法标记（v0.3.6 F2：WM_PAINT 据此画 1px 错误色描边）。
    tier_err: RefCell<[bool; 4]>,
    /// 最近一次自定义字体颜色（v0.3.6：自动→自定义→自动往返保留用户已选色）。
    last_custom_font: Cell<RgbColor>,
    /// 字体（0 = 创建失败回退 DEFAULT_GUI_FONT，销毁时不删）。
    f_base: Cell<isize>,
    f_title: Cell<isize>,
    f_hint: Cell<isize>,
    /// 主题画刷句柄（0 = 无效）。
    br_window: Cell<isize>,
    br_card: Cell<isize>,
    br_input: Cell<isize>,
    br_border: Cell<isize>,
    /// tooltip 控件句柄。
    tooltip: Cell<isize>,
}

impl Ctx {
    /// 逻辑像素 → 设备像素。
    fn sc(&self, v: i32) -> i32 {
        v * self.dpi.get() as i32 / 96
    }

    /// 取字体句柄（0 = 库存字体兜底）。
    fn font(&self, cell: &Cell<isize>) -> HFONT {
        let v = cell.get();
        if v != 0 {
            HFONT(v as *mut core::ffi::c_void)
        } else {
            // SAFETY(E9): GetStockObject 返回系统拥有的库存 GDI 对象句柄，不销毁、不转移所有权。
            unsafe { HFONT(GetStockObject(DEFAULT_GUI_FONT).0) }
        }
    }

    fn brush(&self, cell: &Cell<isize>) -> HBRUSH {
        HBRUSH(cell.get() as *mut core::ffi::c_void)
    }

    /// 即时生效：写入共享配置 + 持久化 TOML（无确定/取消）。
    fn apply(&self, cfg: &Config) {
        // v0.3.14（红蓝共识 B9 必修）：设置应用落 bootlog（含键级 diff）——
        // 此前 apply 零日志，真机反馈「改了不生效」时无从确认是否到达。
        let prev = self.arc.read().map(|c| c.clone()).unwrap_or_default();
        if let Ok(mut guard) = self.arc.write() {
            *guard = cfg.clone();
        }
        // v0.3.17（一轮 P0-⑤）：配置落盘改 tmp→rename 原子写，失败落
        // bootlog 不再静默（写中途崩溃不留半截 TOML）。
        if let Err(e) = crate::write_tmp_rename(&self.path, &cfg.to_toml()) {
            crate::bootlog::note(&format!(
                "settings persist to {} failed: {e}",
                self.path.display()
            ));
        }
        crate::bootlog::note(&settings_applied_note(&prev, cfg));
    }
}

/// 打开（或前置）设置窗口。`preview_text` 为预览数字（如当前电量文本）。
/// v0.3.18（U-5）：`tray_size` 为托盘图标当前 DPI 档位位图边长——预览
/// 走单一渲染源，须与任务栏同尺寸渲染再放大，否则预览与真机不对应。
pub fn open_settings(
    arc: &Arc<RwLock<Config>>,
    path: &Path,
    dark_theme: bool,
    tray_size: u32,
    preview_text: &str,
) {
    // v0.3.7（D-63）：设置窗口注册/创建是启动后关键路径之一，失败留痕。
    crate::bootlog::stage("settings-open");
    let existing = SETTINGS_HWND.load(Ordering::SeqCst);
    if existing != 0 {
        let hwnd = HWND(existing as *mut core::ffi::c_void);
        // SAFETY(E9): hwnd 由 SETTINGS_HWND 原子值还原；先经 IsWindow 校验存活再置前台，校验失败路径不再触碰该句柄。
        unsafe {
            if IsWindow(Some(hwnd)).as_bool() {
                let _ = SetForegroundWindow(hwnd);
                return;
            }
        }
    }

    // v0.3.8（D-64/B-2）：启动时图标渲染回退过出厂默认 → 首次打开设置
    // 窗口时提示一次（取走后不再重复），原因即 bootlog 里的具体失败原因。
    if let Ok(mut slot) = crate::APPEARANCE_FALLBACK_NOTE.lock() {
        if let Some(reason) = slot.take() {
            let text = wide(&format!(
                "外观配置已回退默认（原因：{reason}）。\n\
                 可在下方「外观」分区重新调整字体、颜色、形状与大小。"
            ));
            let caption = wide("LogiVoltTray");
            // SAFETY(E9): text/caption 为 NUL 结尾宽字符缓冲，MessageBoxW 同步调用期间存活。
            unsafe {
                let _ = MessageBoxW(
                    None,
                    PCWSTR(text.as_ptr()),
                    PCWSTR(caption.as_ptr()),
                    MB_OK | MB_ICONWARNING,
                );
            }
        }
    }

    // SAFETY(E9): icc 按值初始化且 dwSize 等于结构体实际大小，符合 InitCommonControlsEx 契约。
    unsafe {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_BAR_CLASSES | ICC_WIN95_CLASSES, // 含 tooltip 类
        };
        let _ = InitCommonControlsEx(&icc);

        let hinst = match GetModuleHandleW(None) {
            Ok(h) => HINSTANCE(h.0),
            Err(e) => {
                crate::bootlog::note(&format!("settings: GetModuleHandleW failed: {e}"));
                return;
            }
        };
        // v0.3.5 #2（D-53）：EXE 版本资源中的 1 号图标（winres set_icon → ID 1）。
        let hicon = LoadIconW(Some(hinst), icon_resource()).unwrap_or(HICON::default());
        let class = wide("VoltTraySettingsWnd");
        let title = wide("LogiVoltTray 设置");
        let wc = WNDCLASSW {
            style: WNDCLASS_STYLES(0),
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinst,
            hIcon: hicon,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or(HCURSOR::default()),
            hbrBackground: HBRUSH::default(), // 背景 WM_ERASEBKGND 自绘（主题色）
            lpszMenuName: PCWSTR::null(),
            lpszClassName: PCWSTR(class.as_ptr()),
        };
        // 类可能已注册（重复打开）：已存在时 ATOM 为 0，属正常，不视为失败。
        let atom = RegisterClassW(&wc);
        if atom == 0 {
            // v0.3.14（红蓝共识 B7 必修）：「类已存在」是重复打开窗口的
            // 正常路径，不再落「类注册失败」字样（真机日志 52 秒 6 条
            // 「注册失败」严重误导诊断）；仅真实失败（其它错误码）留痕。
            let err = GetLastError();
            if err != ERROR_CLASS_ALREADY_EXISTS {
                crate::bootlog::note(&format!("settings: RegisterClassW 类注册失败: {err:?}"));
            }
        }

        // v0.3.6：自定义字体颜色会话内保留（往返切换不丢已选色）。
        let initial_custom = match arc.read() {
            Ok(c) => match c.icon_font_color {
                IconFontColor::Fixed(c) => c,
                IconFontColor::Auto => RgbColor {
                    r: 0xFF,
                    g: 0xFF,
                    b: 0xFF,
                },
            },
            Err(_) => RgbColor {
                r: 0xFF,
                g: 0xFF,
                b: 0xFF,
            },
        };
        // v0.3.22（D-88）：四项基线快照——进程内首次打开设置窗口时记录
        // 一次，用于 hint 行「已修改，重启后生效」动态判定。
        if let Ok(mut slot) = LAB_BASELINE.lock() {
            if slot.is_none() {
                if let Ok(c) = arc.read() {
                    *slot = Some([
                        c.experimental_persist_last_known,
                        c.experimental_stats,
                        c.experimental_per_device_polling,
                        c.experimental_bluetooth_wired,
                    ]);
                }
            }
        }
        let ctx = Box::new(Ctx {
            cfg: RefCell::new(arc.read().map(|c| c.clone()).unwrap_or_default()),
            arc: arc.clone(),
            path: path.to_path_buf(),
            dark_theme,
            tray_size,
            syncing: Cell::new(false),
            preview_text: preview_text.to_string(),
            theme: theme(dark_theme),
            dpi: Cell::new(96),
            cw: Cell::new(L_CLIENT_W),
            expanded: Cell::new(true),
            gate_expanded: Cell::new(false),
            tier_err: RefCell::new([false; 4]),
            last_custom_font: Cell::new(initial_custom),
            f_base: Cell::new(0),
            f_title: Cell::new(0),
            f_hint: Cell::new(0),
            br_window: Cell::new(0),
            br_card: Cell::new(0),
            br_input: Cell::new(0),
            br_border: Cell::new(0),
            tooltip: Cell::new(0),
        });

        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(title.as_ptr()),
            WINDOW_STYLE(SETTINGS_STYLE),
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            500,
            724,
            None,
            None,
            Some(hinst),
            Some(Box::into_raw(ctx) as *const core::ffi::c_void),
        ) {
            Ok(h) => h,
            Err(e) => {
                crate::bootlog::note(&format!("settings: CreateWindowExW failed: {e}"));
                return;
            }
        };
        SETTINGS_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
}

// SAFETY(E9): 窗口过程回调：lparam 在 WM_CREATE 按 CREATESTRUCTW 契约解引用；GWLP_USERDATA 的 Ctx 指针在窗口存活期内有效、由 GUI 线程独占访问。
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let ctx = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Ctx;
    match msg {
        WM_CREATE => {
            let cs = &*(lparam.0 as *const CREATESTRUCTW);
            let raw = cs.lpCreateParams;
            let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);
            let ctx = &mut *(raw as *mut Ctx);
            build_controls(hwnd, ctx);
            LRESULT(0)
        }
        WM_ACTIVATE => {
            // 双向一致：窗口重新激活时以共享配置为准（覆盖外部 TOML 改动）。
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
            // 滑块（发给父窗口；lparam = 滑块 HWND）：低电阈值 + 文本/背景覆盖度。
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
                        // D-57：文本大小 40..=160%，步进 5。
                        let pos = SendMessageW!(ts_ctl, TBM_GETPOS, WPARAM(0), LPARAM(0)) as i32;
                        let v =
                            pos.clamp(ICON_TEXT_SCALE_MIN as i32, ICON_TEXT_SCALE_MAX as i32) as u8;
                        let mut cfg = ctx.cfg.borrow_mut();
                        if cfg.icon_text_scale != v {
                            cfg.icon_text_scale = v;
                            let text = format!("{v}%");
                            set_text(ctx, IDC_TEXT_VAL, &text);
                            ctx.apply(&cfg);
                        }
                    } else if bs_ctl.0 == lp {
                        // D-57：背景覆盖度 20..=100%（100% = 满出血）。
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
        WM_DRAWITEM => {
            // owner-draw 按钮自绘（D-58：悬停/按下态扁平按钮）。
            if ctx.is_null() {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            let dis = &*(lparam.0 as *const DRAWITEMSTRUCT);
            draw_owner_button(&*ctx, dis);
            LRESULT(1)
        }
        WM_ERASEBKGND => {
            if ctx.is_null() {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            let ctx = &*ctx;
            let hdc = HDC(wparam.0 as *mut core::ffi::c_void);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            FillRect(hdc, &rc, ctx.brush(&ctx.br_window));
            LRESULT(1)
        }
        WM_PAINT => {
            if ctx.is_null() {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            let ctx = &*ctx;
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            if !hdc.is_invalid() && ctx.expanded.get() {
                // 分档卡片底（1px 边框 + 卡片底色 + 圆角 4 → 直径 8）。
                let l = ctx.sc(16);
                let t = ctx.sc(166);
                let r = ctx.sc(ctx.cw.get() - 16);
                let b = t + ctx.sc(L_CARD_H);
                let pen = CreatePen(PS_SOLID, 1, COLORREF(ctx.theme.border));
                let old_pen = SelectObject(hdc, HGDIOBJ(pen.0));
                let old_br = SelectObject(hdc, HGDIOBJ(ctx.brush(&ctx.br_card).0));
                let _ = RoundRect(hdc, l, t, r, b, ctx.sc(8), ctx.sc(8));
                SelectObject(hdc, old_br);
                SelectObject(hdc, old_pen);
                let _ = DeleteObject(HGDIOBJ(pen.0));
                // v0.3.6 F2：非法阈值框 1px 错误色描边（沿控件外缘绘制）。
                let errs = ctx.tier_err.borrow();
                for (i, bad) in errs.iter().enumerate() {
                    if !*bad {
                        continue;
                    }
                    let edit = ctl(hwnd, IDC_TIER_TH0 + i as i32);
                    if edit.0.is_null() {
                        continue;
                    }
                    let mut rc = RECT::default();
                    let _ = GetWindowRect(edit, &mut rc);
                    let mut p1 = POINT {
                        x: rc.left,
                        y: rc.top,
                    };
                    let mut p2 = POINT {
                        x: rc.right,
                        y: rc.bottom,
                    };
                    let _ = ScreenToClient(hwnd, &mut p1);
                    let _ = ScreenToClient(hwnd, &mut p2);
                    let pen = CreatePen(PS_SOLID, 1, COLORREF(ctx.theme.error));
                    let old_pen = SelectObject(hdc, HGDIOBJ(pen.0));
                    let old_br = SelectObject(hdc, GetStockObject(NULL_BRUSH));
                    let _ = Rectangle(hdc, p1.x - 1, p1.y - 1, p2.x + 1, p2.y + 1);
                    SelectObject(hdc, old_br);
                    SelectObject(hdc, old_pen);
                    let _ = DeleteObject(HGDIOBJ(pen.0));
                }
            }
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN => {
            if ctx.is_null() {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            let ctx = &*ctx;
            let hdc = HDC(wparam.0 as *mut core::ffi::c_void);
            let child = HWND(lparam.0 as *mut core::ffi::c_void);
            let id = GetDlgCtrlID(child);
            let hint = id == IDC_TIER_HINT
                || id == IDC_COVER_HINT
                || id == IDC_INTERVAL_HINT
                || id == IDC_EXPERIMENTAL_HINT
                || id == IDC_GATE_NOTE
                || id == IDC_LAB_PERSIST_DESC
                || id == IDC_LAB_STATS_DESC
                || id == IDC_LAB_POLLING_DESC
                || id == IDC_LAB_BT_DESC
                || id == IDC_TEXT_RANGE
                || id == IDC_BG_RANGE
                || id == IDC_LOW_RANGE;
            // 分档卡内标签（D-58）：卡片底色而非窗口底色。
            let in_card = matches!(
                id,
                IDC_LBL_TIER_COUNT | IDC_LBL_TIER_TH | IDC_LBL_TIER_COLOR | IDC_TIER_ERR
            ) || (IDC_TIER_TH0..IDC_TIER_DOT0).contains(&id)
                || (IDC_TIER_DOT0..IDC_TIER_DOT0 + 5).contains(&id);
            // 卡内错误提示承载告警 → error 色；提示行 secondary；其余正文 text。
            // （v0.3.18 U-6：对比度警告状态行已删除。）
            let color = if id == IDC_TIER_ERR {
                ctx.theme.error
            } else if hint {
                ctx.theme.secondary
            } else {
                ctx.theme.text
            };
            SetTextColor(hdc, COLORREF(color));
            SetBkMode(hdc, TRANSPARENT);
            let br = if in_card {
                ctx.brush(&ctx.br_card)
            } else {
                ctx.brush(&ctx.br_window)
            };
            if br.is_invalid() {
                LRESULT(0)
            } else {
                LRESULT(br.0 as isize)
            }
        }
        WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX => {
            if ctx.is_null() {
                return DefWindowProcW(hwnd, msg, wparam, lparam);
            }
            let ctx = &*ctx;
            let hdc = HDC(wparam.0 as *mut core::ffi::c_void);
            SetTextColor(hdc, COLORREF(ctx.theme.text));
            let br = ctx.brush(&ctx.br_input);
            if br.is_invalid() {
                LRESULT(0)
            } else {
                LRESULT(br.0 as isize)
            }
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY | WM_NCDESTROY => {
            if msg == WM_NCDESTROY {
                if !ctx.is_null() {
                    let ctx = &*ctx;
                    // 释放自建 GDI 对象（0 = 库存兜底，跳过）。
                    for v in [
                        ctx.f_base.get(),
                        ctx.f_title.get(),
                        ctx.f_hint.get(),
                        ctx.br_window.get(),
                        ctx.br_card.get(),
                        ctx.br_input.get(),
                        ctx.br_border.get(),
                    ] {
                        if v != 0 {
                            let _ = DeleteObject(HGDIOBJ(v as *mut core::ffi::c_void));
                        }
                    }
                    drop(Box::from_raw(ctx as *const Ctx as *mut Ctx));
                }
                let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                SETTINGS_HWND.store(0, Ordering::SeqCst);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ---------------------------------------------------------------------------
// 滚轮/键盘子类化（D-54；windows crate 0.62 无 SetWindowSubclass，用 GWLP_WNDPROC）
// ---------------------------------------------------------------------------

/// 取回子类化前保存的原窗口过程（挂在控件 GWLP_USERDATA 上）。
// SAFETY(E9): 仅在已被 subclass() 子类化的控件上调用；GWLP_USERDATA 存的是原窗口过程地址，transmute 方向与写入时一致。
unsafe fn prev_proc(hwnd: HWND) -> WNDPROC {
    std::mem::transmute::<isize, WNDPROC>(GetWindowLongPtrW(hwnd, GWLP_USERDATA))
}

// SAFETY(E9): prev 为 prev_proc 取得的有效原窗口过程；参数原样转交 CallWindowProcW，符合其调用契约。
unsafe fn call_prev(
    prev: WNDPROC,
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if prev.is_some() {
        CallWindowProcW(prev, hwnd, msg, wparam, lparam)
    } else {
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}

/// WM_MOUSEWHEEL 的滚动增量（wParam 高 16 位有符号）。
fn wheel_delta(wparam: WPARAM) -> i16 {
    (((wparam.0 >> 16) & 0xFFFF) as u16) as i16
}

/// 滑块滚轮：上滚增加、下滚减少（D-54，规范裁决 5）；步进 5 并转发 WM_HSCROLL。
// SAFETY(E9): 子类窗口过程回调：仅处理滚轮/滚动消息，其余一律经 call_prev 交还原窗口过程，不吞消息。
unsafe extern "system" fn trackbar_wheel_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let prev = prev_proc(hwnd);
    if msg == WM_MOUSEWHEEL {
        let d = wheel_delta(wparam);
        let lo = SendMessageW!(hwnd, TBM_GETRANGEMIN, WPARAM(0), LPARAM(0)) as i32;
        let hi = SendMessageW!(hwnd, TBM_GETRANGEMAX, WPARAM(0), LPARAM(0)) as i32;
        let pos = SendMessageW!(hwnd, TBM_GETPOS, WPARAM(0), LPARAM(0)) as i32;
        let next = if d > 0 { pos + 5 } else { pos - 5 }.clamp(lo, hi);
        if next != pos {
            SendMessageW!(hwnd, TBM_SETPOS, WPARAM(1), LPARAM(next as isize));
            if let Ok(parent) = GetParent(hwnd) {
                SendMessageW!(
                    parent,
                    WM_HSCROLL,
                    WPARAM(TB_THUMBPOSITION as usize),
                    LPARAM(hwnd.0 as isize)
                );
            }
        }
        return LRESULT(0);
    }
    call_prev(prev, hwnd, msg, wparam, lparam)
}

/// 阈值数字框：滚轮 ±1、↑/↓ 键 ±1（1–99），写入文本触发 EN_CHANGE 交父窗口校验。
// SAFETY(E9): 子类窗口过程回调：仅处理滚轮/按键消息，其余一律经 call_prev 交还原窗口过程，不吞消息。
unsafe extern "system" fn edit_wheel_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let prev = prev_proc(hwnd);
    let is_wheel_step = msg == WM_MOUSEWHEEL;
    let is_key_step = msg == WM_KEYDOWN && (wparam.0 == VK_UP || wparam.0 == VK_DOWN);
    if is_wheel_step || is_key_step {
        let up = if is_wheel_step {
            wheel_delta(wparam) > 0
        } else {
            wparam.0 == VK_UP
        };
        let cur = edit_text(hwnd).parse::<i64>().unwrap_or(-1);
        if (FONT_TIER_THRESHOLD_MIN as i64..=FONT_TIER_THRESHOLD_MAX as i64).contains(&cur) {
            let next = (cur + if up { 1 } else { -1 }).clamp(
                FONT_TIER_THRESHOLD_MIN as i64,
                FONT_TIER_THRESHOLD_MAX as i64,
            );
            if next != cur {
                let v = wide(&next.to_string());
                let _ = SetWindowTextW(hwnd, PCWSTR(v.as_ptr())); // 触发 EN_CHANGE
            }
        }
        return LRESULT(0);
    }
    call_prev(prev, hwnd, msg, wparam, lparam)
}

/// owner-draw 按钮悬停态：进入/离开时请求重绘（hover 由 WM_DRAWITEM 即时判定）。
// SAFETY(E9): 子类窗口过程回调：仅处理鼠标移动/离开消息，其余一律经 call_prev 交还原窗口过程，不吞消息。
unsafe extern "system" fn button_hover_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let prev = prev_proc(hwnd);
    if msg == WM_MOUSEMOVE {
        let mut tme = TRACKMOUSEEVENT {
            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_HOVER | TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: 10,
        };
        let _ = TrackMouseEvent(&mut tme);
    } else if msg == WM_MOUSEHOVER || msg == WM_MOUSELEAVE {
        let _ = InvalidateRect(Some(hwnd), None, true);
    }
    call_prev(prev, hwnd, msg, wparam, lparam)
}

/// GWLP_WNDPROC 子类化（原过程存 GWLP_USERDATA）。
// SAFETY(E9): hwnd 非空已校验；先把原过程地址存入 GWLP_USERDATA 再替换 GWLP_WNDPROC，保证任一时刻都可取回有效原过程。
unsafe fn subclass(hwnd: HWND, proc_: WNDPROC) {
    if hwnd.0.is_null() {
        return;
    }
    let new_raw = std::mem::transmute::<WNDPROC, isize>(proc_);
    let prev_raw = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, new_raw);
    let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, prev_raw);
}

// ---------------------------------------------------------------------------
// 控件创建与布局
// ---------------------------------------------------------------------------

// SAFETY(E9): class/title 宽字符缓冲 NUL 结尾且存活至 CreateWindowExW 返回；句柄无效时返回 None，不产生半初始化控件。
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
    hinst: HINSTANCE,
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

/// Segoe UI 字体（pt_x10 = 磅 ×10；失败返回 None 由库存字体兜底）。
// SAFETY(E9): LOGFONTW 按值构造；字体名拷入定长数组且保持 NUL 结尾（长度已静态约束）；失败返回 None 由库存字体兜底。
unsafe fn make_font(dpi: u32, pt_x10: i32, weight: i32) -> Option<HFONT> {
    let face = wide("Segoe UI"); // 含 NUL，len = 11 ≤ 32
    let mut name = [0u16; 32];
    name[..face.len()].copy_from_slice(&face[..]);
    let lf = LOGFONTW {
        lfHeight: -((pt_x10 * dpi as i32 + 360) / 720),
        lfWeight: weight,
        lfCharSet: DEFAULT_CHARSET,
        lfFaceName: name,
        ..Default::default()
    };
    let f = CreateFontIndirectW(&lf);
    if f.is_invalid() {
        None
    } else {
        Some(f)
    }
}

/// MAKEINTRESOURCE(1)：图标资源 ID=1，指针位模式即 ID（非悬垂指针，clippy 误报）。
#[allow(clippy::manual_dangling_ptr)]
fn icon_resource() -> PCWSTR {
    PCWSTR(1usize as *const u16)
}

/// 实测文本宽度（按所选字体，逻辑像素）。
// SAFETY(E9): GetDC 取得的 HDC 用毕 ReleaseDC 归还；SelectObject 选入字体后恢复原对象；文本长度显式传入、不依赖 NUL 终止。
unsafe fn text_width(hwnd: HWND, font: HFONT, s: &str) -> i32 {
    let v: Vec<u16> = s.encode_utf16().collect();
    let hdc = GetDC(Some(hwnd));
    if hdc.is_invalid() {
        return 0;
    }
    let old = SelectObject(hdc, HGDIOBJ(font.0));
    let mut sz = SIZE::default();
    let ok = GetTextExtentPoint32W(hdc, &v, &mut sz).as_bool();
    SelectObject(hdc, old);
    ReleaseDC(Some(hwnd), hdc);
    if ok {
        sz.cx
    } else {
        0
    }
}

/// 生成 w×h swatch 位图（BGRA：圆角 2、1px 边框、实心填充；边缘 1px 抗锯齿）。
/// `border` 为 COLORREF（0x00BBGGRR），来自 Theme token。
fn swatch_bgra(c: RgbColor, border: u32, w: i32, h: i32) -> Vec<u8> {
    let bb = (border & 0xFF) as u8;
    let bg_c = ((border >> 8) & 0xFF) as u8;
    let br_c = ((border >> 16) & 0xFF) as u8;
    let r = 2.0f32;
    let mut v = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let fx = x as f32;
            let fy = y as f32;
            let mut cov = 1.0f32;
            let corners = [
                (r, r),
                (w as f32 - 1.0 - r, r),
                (r, h as f32 - 1.0 - r),
                (w as f32 - 1.0 - r, h as f32 - 1.0 - r),
            ];
            for &(cx, cy) in &corners {
                let in_x = if cx < w as f32 / 2.0 {
                    fx < cx
                } else {
                    fx > cx
                };
                let in_y = if cy < h as f32 / 2.0 {
                    fy < cy
                } else {
                    fy > cy
                };
                if in_x && in_y {
                    let d = ((fx - cx).powi(2) + (fy - cy).powi(2)).sqrt();
                    if d > r {
                        cov = 0.0;
                    } else if d > r - 1.0 {
                        cov = cov.min((r - d).clamp(0.0, 1.0));
                    }
                }
            }
            let outer = x == 0 || y == 0 || x == w - 1 || y == h - 1;
            let (b, g, rr, a) = if cov <= 0.0 {
                (0u8, 0u8, 0u8, 0u8)
            } else if outer && cov < 1.0 {
                // 圆角过渡：填充色半透明（预乘由 DIB 打包统一处理）。
                (c.b, c.g, c.r, (cov * 255.0) as u8)
            } else if outer {
                (bb, bg_c, br_c, 255)
            } else {
                (c.b, c.g, c.r, 255)
            };
            let i = ((y * w + x) * 4) as usize;
            v[i] = b;
            v[i + 1] = g;
            v[i + 2] = rr;
            v[i + 3] = a;
        }
    }
    v
}

// SAFETY(E9): 全部子控件经 create_ctl 创建并登记于 ctx；任一创建失败即返回，不留下半初始化界面状态。
unsafe fn build_controls(hwnd: HWND, ctx: &mut Ctx) {
    let hinst = match GetModuleHandleW(None) {
        Ok(h) => HINSTANCE(h.0),
        Err(_) => return,
    };
    let dpi = GetDpiForWindow(hwnd);
    ctx.dpi.set(dpi);

    // 字体（规范裁决 6：正文 9pt / 分区标题 10.5pt Semibold / 提示 8pt）。
    if let Some(f) = make_font(dpi, 90, 400) {
        ctx.f_base.set(f.0 as isize);
    }
    if let Some(f) = make_font(dpi, 105, 600) {
        ctx.f_title.set(f.0 as isize);
    }
    if let Some(f) = make_font(dpi, 80, 400) {
        ctx.f_hint.set(f.0 as isize);
    }
    // 主题画刷。
    ctx.br_window
        .set(CreateSolidBrush(COLORREF(ctx.theme.window)).0 as isize);
    ctx.br_card
        .set(CreateSolidBrush(COLORREF(ctx.theme.card)).0 as isize);
    ctx.br_input.set(
        CreateSolidBrush(COLORREF(if ctx.dark_theme {
            ctx.theme.card
        } else {
            ctx.theme.window
        }))
        .0 as isize,
    );
    ctx.br_border
        .set(CreateSolidBrush(COLORREF(ctx.theme.border)).0 as isize);

    let f_base = ctx.font(&ctx.f_base);
    let f_title = ctx.font(&ctx.f_title);
    let f_hint = ctx.font(&ctx.f_hint);

    // 裁决 3：以最长提示语实测宽度反推客户区宽，杜绝截断。
    // （v0.3.18 U-6：对比度警告文案样本随警告条一并删除。）
    let tier_hint_w = text_width(hwnd, f_hint, TIER_HINT_SAMPLE);
    let cw = L_CLIENT_W.max(tier_hint_w + 24 + 32); // 卡片内提示（x=28，内宽 = cw-56）
    ctx.cw.set(cw);

    // 窗口图标（#2：大/小图标 + 类图标，任务栏/Alt-Tab 显示）。
    if let Ok(hicon) = LoadIconW(Some(hinst), icon_resource()) {
        SendMessageW!(
            hwnd,
            WM_SETICON,
            WPARAM(ICON_SMALL_ID),
            LPARAM(hicon.0 as isize)
        );
        SendMessageW!(
            hwnd,
            WM_SETICON,
            WPARAM(ICON_BIG_ID),
            LPARAM(hicon.0 as isize)
        );
    }
    // 深色标题栏（D-58）。
    if ctx.dark_theme {
        let v: i32 = 1;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWINDOWATTRIBUTE(DWMWA_USE_IMMERSIVE_DARK_MODE),
            &v as *const i32 as *const core::ffi::c_void,
            4,
        );
    }

    let sc = |v: i32| (v * dpi as i32) / 96;

    // —— 分区标题：外观（固定位置，不随分档卡移动）——
    create_ctl(
        hwnd,
        "STATIC",
        Some("外观"),
        ws(0),
        WINDOW_EX_STYLE(0),
        sc(16),
        sc(12),
        sc(80),
        sc(18),
        0,
        hinst,
        f_title,
    );

    // —— 固定区行（形状 / 背景色 / 字体 / 字体颜色）——
    create_ctl(
        hwnd,
        "STATIC",
        Some("形状"),
        ws(0),
        WINDOW_EX_STYLE(0),
        sc(16),
        sc(38),
        sc(64),
        sc(20),
        0,
        hinst,
        f_base,
    );
    create_ctl(
        hwnd,
        "COMBOBOX",
        None,
        ws(CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0),
        WINDOW_EX_STYLE(0),
        sc(88),
        sc(34),
        sc(180),
        sc(220),
        IDC_SHAPE,
        hinst,
        f_base,
    );
    create_ctl(
        hwnd,
        "STATIC",
        Some("背景色"),
        ws(0),
        WINDOW_EX_STYLE(0),
        sc(16),
        sc(70),
        sc(64),
        sc(20),
        0,
        hinst,
        f_base,
    );
    create_ctl(
        hwnd,
        "COMBOBOX",
        None,
        ws(CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0),
        WINDOW_EX_STYLE(0),
        sc(88),
        sc(66),
        sc(140),
        sc(220),
        IDC_BG,
        hinst,
        f_base,
    );
    create_ctl(
        hwnd,
        "STATIC",
        None,
        ws(SS_BITMAP),
        WINDOW_EX_STYLE(0),
        sc(236),
        sc(66),
        sc(40),
        sc(24),
        IDC_BG_SWATCH,
        hinst,
        f_base,
    );
    create_ctl(
        hwnd,
        "BUTTON",
        Some("选色…"),
        ws(BS_OWNERDRAW_STYLE),
        WINDOW_EX_STYLE(0),
        sc(284),
        sc(64),
        sc(80),
        sc(24),
        IDC_BG_PICK,
        hinst,
        f_base,
    );
    create_ctl(
        hwnd,
        "STATIC",
        Some("字体"),
        ws(0),
        WINDOW_EX_STYLE(0),
        sc(16),
        sc(102),
        sc(64),
        sc(20),
        0,
        hinst,
        f_base,
    );
    create_ctl(
        hwnd,
        "COMBOBOX",
        None,
        ws(CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0),
        WINDOW_EX_STYLE(0),
        sc(88),
        sc(98),
        sc(240),
        sc(260),
        IDC_FONT_FAMILY,
        hinst,
        f_base,
    );
    create_ctl(
        hwnd,
        "STATIC",
        Some("字体颜色"),
        ws(0),
        WINDOW_EX_STYLE(0),
        sc(16),
        sc(134),
        sc(64),
        sc(20),
        0,
        hinst,
        f_base,
    );
    // 裁决 1：字体颜色下拉 = [自动（按电量档位）, 自定义]，宽 ≥200px。
    create_ctl(
        hwnd,
        "COMBOBOX",
        None,
        ws(CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0),
        WINDOW_EX_STYLE(0),
        sc(88),
        sc(130),
        sc(200),
        sc(220),
        IDC_FONT_COLOR,
        hinst,
        f_base,
    );
    create_ctl(
        hwnd,
        "STATIC",
        None,
        ws(SS_BITMAP),
        WINDOW_EX_STYLE(0),
        sc(296),
        sc(130),
        sc(40),
        sc(24),
        IDC_FONT_SWATCH,
        hinst,
        f_base,
    );
    create_ctl(
        hwnd,
        "BUTTON",
        Some("选色…"),
        ws(BS_OWNERDRAW_STYLE),
        WINDOW_EX_STYLE(0),
        sc(344),
        sc(128),
        sc(80),
        sc(24),
        IDC_FONT_PICK,
        hinst,
        f_base,
    );

    // —— 分档卡（展开态位于 y=166；收起时隐藏整组并回收空间）——
    create_ctl(
        hwnd,
        "STATIC",
        Some("档数"),
        ws(0),
        WINDOW_EX_STYLE(0),
        sc(28),
        sc(178),
        sc(44),
        sc(20),
        IDC_LBL_TIER_COUNT,
        hinst,
        f_base,
    );
    create_ctl(
        hwnd,
        "COMBOBOX",
        None,
        ws(CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0),
        WINDOW_EX_STYLE(0),
        sc(80),
        sc(174),
        sc(88),
        sc(140),
        IDC_TIER_COUNT,
        hinst,
        f_base,
    );
    create_ctl(
        hwnd,
        "STATIC",
        Some("阈值"),
        ws(0),
        WINDOW_EX_STYLE(0),
        sc(28),
        sc(206),
        sc(44),
        sc(20),
        IDC_LBL_TIER_TH,
        hinst,
        f_base,
    );
    for i in 0..4 {
        create_ctl(
            hwnd,
            "EDIT",
            None,
            ws(ES_NUMBER),
            WINDOW_EX_STYLE(WS_EX_CLIENTEDGE_RAW),
            sc(80 + i * 48),
            sc(202),
            sc(40),
            sc(22),
            IDC_TIER_TH0 + i,
            hinst,
            f_base,
        );
    }
    // v0.3.6 F2：分档卡内右侧红色错误提示（阈值非法反馈不占状态行）。
    create_ctl(
        hwnd,
        "STATIC",
        None,
        ws(0),
        WINDOW_EX_STYLE(0),
        sc(cw - 216),
        sc(206),
        sc(200),
        sc(20),
        IDC_TIER_ERR,
        hinst,
        f_base,
    );
    create_ctl(
        hwnd,
        "STATIC",
        Some("颜色"),
        ws(0),
        WINDOW_EX_STYLE(0),
        sc(28),
        sc(234),
        sc(44),
        sc(20),
        IDC_LBL_TIER_COLOR,
        hinst,
        f_base,
    );
    for i in 0..5 {
        create_ctl(
            hwnd,
            "STATIC",
            None,
            ws(SS_BITMAP | SS_NOTIFY),
            WINDOW_EX_STYLE(0),
            sc(80 + i * 24),
            sc(234),
            sc(DOT_PX),
            sc(DOT_PX),
            IDC_TIER_DOT0 + i,
            hinst,
            f_base,
        );
    }
    // v0.3.6：分档说明示例按当前档数/阈值实时拼写（P3 规范偏差修正）。
    let tier_hint_init = tier_hint_dynamic(&ctx.cfg.borrow().icon_font_tier_thresholds);
    create_ctl(
        hwnd,
        "STATIC",
        Some(&tier_hint_init),
        ws(0),
        WINDOW_EX_STYLE(0),
        sc(28),
        sc(258),
        sc(cw - 56),
        sc(14),
        IDC_TIER_HINT,
        hinst,
        f_hint,
    );

    // —— BELOW 表控件（展开态坐标；随后 relayout 决定最终位置/可见性）——
    // v0.3.6：滑块行最小—最大范围标注（范围取自 config 常量，单一来源）。
    let text_range = format!("{ICON_TEXT_SCALE_MIN}–{ICON_TEXT_SCALE_MAX}");
    let bg_range = format!("{ICON_BG_SCALE_MIN}–{ICON_BG_SCALE_MAX}");
    let low_range = format!("{LOW_THRESHOLD_MIN}–{LOW_THRESHOLD_MAX}");
    for &(id, x, y, w, h) in BELOW.iter() {
        let (class, style, ex, title): (&str, WINDOW_STYLE, WINDOW_EX_STYLE, Option<&str>) =
            match id {
                IDC_TEXT_SLIDER | IDC_BG_SLIDER | IDC_SLIDER => (
                    "msctls_trackbar32",
                    ws(TBS_AUTOTICKS),
                    WINDOW_EX_STYLE(0),
                    None,
                ),
                IDC_TEXT_VAL | IDC_BG_VAL | IDC_THRESH_VAL => {
                    ("STATIC", ws(0), WINDOW_EX_STYLE(0), Some("0%"))
                }
                IDC_LBL_TEXT => ("STATIC", ws(0), WINDOW_EX_STYLE(0), Some("文本大小")),
                IDC_LBL_BGCOVER => ("STATIC", ws(0), WINDOW_EX_STYLE(0), Some("背景覆盖度")),
                IDC_COVER_HINT => ("STATIC", ws(0), WINDOW_EX_STYLE(0), Some(COVER_HINT_TEXT)),
                // v0.3.18（U-3）：标签固定单行「预览」，不再标注倍数。
                IDC_LBL_PREVIEW_BIG => ("STATIC", ws(0), WINDOW_EX_STYLE(0), Some("预览")),
                IDC_PREVIEW_BIG => ("STATIC", ws(SS_BITMAP), WINDOW_EX_STYLE(0), None),
                IDC_SEC_REFRESH => ("STATIC", ws(0), WINDOW_EX_STYLE(0), Some("刷新")),
                IDC_SEC_LOW => ("STATIC", ws(0), WINDOW_EX_STYLE(0), Some("低电提醒")),
                IDC_LBL_INTERVAL => ("STATIC", ws(0), WINDOW_EX_STYLE(0), Some("轮询间隔")),
                IDC_INTERVAL => (
                    "COMBOBOX",
                    ws(CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0),
                    WINDOW_EX_STYLE(0),
                    None,
                ),
                IDC_INTERVAL_HINT => (
                    "STATIC",
                    ws(0),
                    WINDOW_EX_STYLE(0),
                    Some(INTERVAL_HINT_TEXT),
                ),
                IDC_EXPERIMENTAL_HINT => ("STATIC", ws(0), WINDOW_EX_STYLE(0), Some(LAB_HINT_TEXT)),
                IDC_BLINK => (
                    "BUTTON",
                    ws(BS_AUTOCHECKBOX),
                    WINDOW_EX_STYLE(0),
                    Some("低电时图标闪烁提醒"),
                ),
                IDC_LBL_LOW => ("STATIC", ws(0), WINDOW_EX_STYLE(0), Some("低电阈值")),
                IDC_TEXT_RESET | IDC_BG_RESET | IDC_RESET_LOW => (
                    "BUTTON",
                    ws(BS_OWNERDRAW_STYLE),
                    WINDOW_EX_STYLE(0),
                    Some("重置"),
                ),
                IDC_RESET_ALL => (
                    "BUTTON",
                    ws(BS_OWNERDRAW_STYLE),
                    WINDOW_EX_STYLE(0),
                    Some("恢复全部默认"),
                ),
                IDC_TEXT_RANGE => ("STATIC", ws(0), WINDOW_EX_STYLE(0), Some(&text_range)),
                IDC_BG_RANGE => ("STATIC", ws(0), WINDOW_EX_STYLE(0), Some(&bg_range)),
                IDC_LOW_RANGE => ("STATIC", ws(0), WINDOW_EX_STYLE(0), Some(&low_range)),
                // v0.3.22（D-87）：精细化设置闸门——复用 IDC_BLINK 的
                // BS_AUTOCHECKBOX 链路；计数由 sync_controls 动态回填。
                IDC_FINE_TUNE_GATE => (
                    "BUTTON",
                    ws(BS_AUTOCHECKBOX),
                    WINDOW_EX_STYLE(0),
                    Some("精细化设置（试验功能）"),
                ),
                IDC_GATE_NOTE => ("STATIC", ws(0), WINDOW_EX_STYLE(0), Some(GATE_NOTE_TEXT)),
                // 实验室分组框（BS_GROUPBOX）+ 四项白话复选框与说明行。
                IDC_LAB_GROUP => (
                    "BUTTON",
                    ws(BS_GROUPBOX_STYLE),
                    WINDOW_EX_STYLE(0),
                    Some(LAB_GROUP_TITLE),
                ),
                IDC_LAB_PERSIST | IDC_LAB_STATS | IDC_LAB_POLLING | IDC_LAB_BT => {
                    let label = LAB_ITEMS
                        .iter()
                        .find(|(i, _, _)| *i == id)
                        .map(|(_, l, _)| *l)
                        .unwrap_or("");
                    (
                        "BUTTON",
                        ws(BS_AUTOCHECKBOX),
                        WINDOW_EX_STYLE(0),
                        Some(label),
                    )
                }
                IDC_LAB_PERSIST_DESC | IDC_LAB_STATS_DESC | IDC_LAB_POLLING_DESC
                | IDC_LAB_BT_DESC => {
                    let desc = LAB_ITEMS
                        .iter()
                        .find(|(i, _, _)| *i == id - 4)
                        .map(|(_, _, d)| *d)
                        .unwrap_or("");
                    ("STATIC", ws(0), WINDOW_EX_STYLE(0), Some(desc))
                }
                IDC_LAB_RESET => (
                    "BUTTON",
                    ws(BS_OWNERDRAW_STYLE),
                    WINDOW_EX_STYLE(0),
                    Some("全部恢复默认（开启）"),
                ),
                IDC_LAB_RESTART => (
                    "BUTTON",
                    ws(BS_OWNERDRAW_STYLE),
                    WINDOW_EX_STYLE(0),
                    Some("立即重启"),
                ),
                _ => continue,
            };
        let font = match id {
            IDC_SEC_REFRESH | IDC_SEC_LOW => f_title,
            IDC_COVER_HINT | IDC_INTERVAL_HINT | IDC_EXPERIMENTAL_HINT => f_hint,
            IDC_GATE_NOTE | IDC_LAB_PERSIST_DESC | IDC_LAB_STATS_DESC | IDC_LAB_POLLING_DESC
            | IDC_LAB_BT_DESC => f_hint,
            IDC_TEXT_RANGE | IDC_BG_RANGE | IDC_LOW_RANGE => f_hint,
            _ => f_base,
        };
        create_ctl(
            hwnd,
            class,
            title,
            style,
            ex,
            sc(x),
            sc(y),
            sc(w),
            sc(h),
            id,
            hinst,
            font,
        );
    }
    // —— 组合框内容 ——
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
    let count = GetDlgItem(Some(hwnd), IDC_TIER_COUNT).unwrap_or_default();
    for k in FONT_TIER_MIN_TIERS..=FONT_TIER_MAX_TIERS {
        add_string(count, &format!("{k} 档"));
    }
    let fam = GetDlgItem(Some(hwnd), IDC_FONT_FAMILY).unwrap_or_default();
    for name in enum_fonts() {
        add_string(fam, &name);
    }
    let iv = GetDlgItem(Some(hwnd), IDC_INTERVAL).unwrap_or_default();
    // 2026-10-25 用户反馈：秒不易读——下拉文案改人性化单位（分钟/小时），
    // 显示层专用；选中档仍以秒经 POLL_TIERS_SECS 索引回写配置（见下 CBN_SELCHANGE）。
    for secs in POLL_TIERS_SECS {
        add_string(iv, &poll_tier_label(secs));
    }

    // —— 滑块范围（常量统一来自 config；D-57）——
    let slider = GetDlgItem(Some(hwnd), IDC_SLIDER).unwrap_or_default();
    SendMessageW!(
        slider,
        TBM_SETRANGE,
        WPARAM(1),
        LPARAM(((LOW_THRESHOLD_MAX as isize) << 16) | LOW_THRESHOLD_MIN as isize)
    );
    SendMessageW!(slider, TBM_SETTICFREQ, WPARAM(5), LPARAM(0));
    let ts = GetDlgItem(Some(hwnd), IDC_TEXT_SLIDER).unwrap_or_default();
    SendMessageW!(
        ts,
        TBM_SETRANGE,
        WPARAM(1),
        LPARAM(((ICON_TEXT_SCALE_MAX as isize) << 16) | ICON_TEXT_SCALE_MIN as isize)
    );
    SendMessageW!(ts, TBM_SETTICFREQ, WPARAM(5), LPARAM(0));
    let bs = GetDlgItem(Some(hwnd), IDC_BG_SLIDER).unwrap_or_default();
    SendMessageW!(
        bs,
        TBM_SETRANGE,
        WPARAM(1),
        LPARAM(((ICON_BG_SCALE_MAX as isize) << 16) | ICON_BG_SCALE_MIN as isize)
    );
    SendMessageW!(bs, TBM_SETTICFREQ, WPARAM(5), LPARAM(0));

    // —— 滚轮/键盘子类化（D-54）——
    subclass(slider, Some(trackbar_wheel_proc));
    subclass(ts, Some(trackbar_wheel_proc));
    subclass(bs, Some(trackbar_wheel_proc));
    for i in 0..4 {
        let e = GetDlgItem(Some(hwnd), IDC_TIER_TH0 + i).unwrap_or_default();
        subclass(e, Some(edit_wheel_proc));
    }
    // owner-draw 按钮悬停重绘。
    for id in [
        IDC_BG_PICK,
        IDC_FONT_PICK,
        IDC_TEXT_RESET,
        IDC_BG_RESET,
        IDC_RESET_LOW,
        IDC_RESET_ALL,
        IDC_LAB_RESET,
        IDC_LAB_RESTART,
    ] {
        let b = GetDlgItem(Some(hwnd), id).unwrap_or_default();
        subclass(b, Some(button_hover_proc));
    }

    // —— 分档色点 tooltip（裁决 2：hover 显示档位区间 + HEX）——
    let tip_class = wide("tooltips_class32");
    let tip = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(tip_class.as_ptr()),
        PCWSTR::null(),
        WINDOW_STYLE(WS_POPUP.0 | TTS_FLAGS_RAW),
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        0,
        0,
        Some(hwnd),
        None,
        Some(hinst),
        None,
    )
    .unwrap_or_default();
    ctx.tooltip.set(tip.0 as isize);
    if !tip.0.is_null() {
        for i in 0..5 {
            let dot = GetDlgItem(Some(hwnd), IDC_TIER_DOT0 + i).unwrap_or_default();
            tip_tool(tip, hwnd, dot, "", true);
        }
    }

    // 初始布局（按当前配置的字体颜色模式展开/收起分档卡）。
    let tiered = ctx.cfg.borrow().icon_font_tiers_enabled;
    relayout(hwnd, ctx, tiered);
    sync_controls(hwnd, ctx);
}

/// tooltip 增删/更新文本（add=true 走 TTM_ADDTOOLW，否则 TTM_UPDATETIPTEXTW）。
// SAFETY(E9): tip/tool 句柄非空已校验；TOOLINFOW 按值初始化且 cbSize 正确，文本缓冲 NUL 结尾、存活至 SendMessageW 同步返回。
unsafe fn tip_tool(tip: HWND, owner: HWND, tool: HWND, text: &str, add: bool) {
    if tip.0.is_null() || tool.0.is_null() {
        return;
    }
    let mut v = wide(text);
    let ti = TTTOOLINFOW {
        cbSize: std::mem::size_of::<TTTOOLINFOW>() as u32,
        uFlags: TTF_IDISHWND | TTF_SUBCLASS,
        hwnd: owner,
        uId: tool.0 as usize,
        rect: RECT::default(),
        hinst: HINSTANCE::default(),
        lpszText: PWSTR(v.as_mut_ptr()),
        lParam: LPARAM(0),
        lpReserved: std::ptr::null_mut(),
    };
    let msg = if add {
        TTM_ADDTOOLW
    } else {
        TTM_UPDATETIPTEXTW
    };
    SendMessageW!(
        tip,
        msg,
        WPARAM(0),
        LPARAM(&ti as *const TTTOOLINFOW as isize)
    );
}

/// 展开态卡片内控件 id 集（收起时整组隐藏；档内多余阈值框/色点按档数二次隐藏）。
const TIER_CHILDREN: [i32; 14] = [
    IDC_LBL_TIER_COUNT,
    IDC_TIER_COUNT,
    IDC_LBL_TIER_TH,
    IDC_TIER_TH0,
    IDC_TIER_TH0 + 1,
    IDC_TIER_TH0 + 2,
    IDC_TIER_TH0 + 3,
    IDC_TIER_ERR,
    IDC_LBL_TIER_COLOR,
    IDC_TIER_DOT0,
    IDC_TIER_DOT0 + 1,
    IDC_TIER_DOT0 + 2,
    IDC_TIER_DOT0 + 3,
    IDC_TIER_DOT0 + 4,
    // IDC_TIER_HINT 单列（随卡片隐藏）。
];

/// 实验室分组控件集（v0.3.22 D-87：闸门收起时整组隐藏）。
const LAB_CHILDREN: [i32; 12] = [
    IDC_LAB_GROUP,
    IDC_LAB_PERSIST,
    IDC_LAB_STATS,
    IDC_LAB_POLLING,
    IDC_LAB_BT,
    IDC_LAB_PERSIST_DESC,
    IDC_LAB_STATS_DESC,
    IDC_LAB_POLLING_DESC,
    IDC_LAB_BT_DESC,
    IDC_EXPERIMENTAL_HINT,
    IDC_LAB_RESET,
    IDC_LAB_RESTART,
];

/// 布局刷新：分档卡展开/收起 → 下方控件上移 TIER_DELTA，窗口高度同步收缩。
// SAFETY(E9): 仅对本窗口已登记子控件调用 SetWindowPos；坐标全部来自 BELOW 常量表与 TIER_DELTA 推算，无外部输入。
unsafe fn relayout(hwnd: HWND, ctx: &Ctx, expanded: bool) {
    ctx.expanded.set(expanded);
    let dy = if expanded { 0 } else { -TIER_DELTA };
    for &(id, x, y, w, h) in BELOW.iter() {
        let w2 = match id {
            IDC_COVER_HINT | IDC_INTERVAL_HINT => ctx.cw.get() - 104,
            IDC_EXPERIMENTAL_HINT => ctx.cw.get() - 56,
            IDC_GATE_NOTE => ctx.cw.get() - 32,
            IDC_LAB_GROUP => ctx.cw.get() - 32,
            IDC_LAB_PERSIST_DESC | IDC_LAB_STATS_DESC | IDC_LAB_POLLING_DESC | IDC_LAB_BT_DESC => {
                ctx.cw.get() - 76
            }
            IDC_TEXT_RANGE | IDC_BG_RANGE | IDC_LOW_RANGE => ctx.cw.get() - 416,
            _ => w,
        };
        let _ = MoveWindow(
            ctl(hwnd, id),
            ctx.sc(x),
            ctx.sc(y + dy),
            ctx.sc(w2),
            ctx.sc(h),
            true,
        );
    }
    // 分档卡可见性。
    for id in TIER_CHILDREN {
        let _ = ShowWindow(ctl(hwnd, id), if expanded { SW_SHOW } else { SW_HIDE });
    }
    let _ = ShowWindow(
        ctl(hwnd, IDC_TIER_HINT),
        if expanded { SW_SHOW } else { SW_HIDE },
    );
    // v0.3.22（D-87）：实验室分组可见性——闸门收起仅隐藏选项，四项使能
    // 不变（收起≠关闭）。
    let lab_visible = ctx.gate_expanded.get();
    for id in LAB_CHILDREN {
        let _ = ShowWindow(ctl(hwnd, id), if lab_visible { SW_SHOW } else { SW_HIDE });
    }
    // 窗口尺寸（客户区 → 外框）。
    let client_h = if expanded {
        L_EXPANDED_H
    } else {
        L_COLLAPSED_H
    } + if ctx.gate_expanded.get() {
        EXP_GATE_DELTA
    } else {
        0
    };
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: ctx.sc(ctx.cw.get()),
        bottom: ctx.sc(client_h),
    };
    if AdjustWindowRectExForDpi(
        &mut rc,
        WINDOW_STYLE(SETTINGS_STYLE),
        false,
        WINDOW_EX_STYLE(0),
        ctx.dpi.get(),
    )
    .is_ok()
    {
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            rc.right - rc.left,
            rc.bottom - rc.top,
            SWP_NOMOVE | SWP_NOZORDER,
        );
    }
    let _ = InvalidateRect(Some(hwnd), None, true);
}

// SAFETY(E9): GetDC(None) 取得的屏幕 DC 用毕 ReleaseDC；LOGFONTW 按值构造，枚举回调 font_enum 仅在其调用期间收集结果。
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

// SAFETY(E9): 字体枚举回调：lpelf 由系统在回调期间保证有效，仅读取不留存；lparam 还原的收集器指针由 enum_fonts 保证存活至枚举结束。
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
    // SAFETY(E9): v 为 NUL 结尾宽字符缓冲，SendMessageW(CB_ADDSTRING) 同步返回前存活；combo 为本窗口有效句柄。
    unsafe {
        SendMessageW!(combo, CB_ADDSTRING, WPARAM(0), LPARAM(v.as_ptr() as isize));
    }
}

fn combo_sel(combo: HWND) -> i32 {
    // SAFETY(E9): combo 为本窗口有效句柄；SendMessageW(CB_GETCURSEL) 仅传值参数、同步返回。
    unsafe { SendMessageW!(combo, CB_GETCURSEL, WPARAM(0), LPARAM(0)) as i32 }
}

fn combo_set_sel(combo: HWND, idx: i32) {
    // SAFETY(E9): combo 为本窗口有效句柄；CB_SETCURSEL 索引经 max(0) 钳制，仅传值参数。
    unsafe {
        SendMessageW!(combo, CB_SETCURSEL, WPARAM(idx.max(0) as usize), LPARAM(0));
    }
}

fn combo_count(combo: HWND) -> i32 {
    // SAFETY(E9): combo 为本窗口有效句柄；SendMessageW(CB_GETCOUNT) 仅传值参数、同步返回。
    unsafe { SendMessageW!(combo, CB_GETCOUNT, WPARAM(0), LPARAM(0)) as i32 }
}

fn combo_current_text(combo: HWND) -> String {
    // SAFETY(E9): combo 有效句柄；索引取自 CB_GETCURSEL，负值提前返回，不越界读取。
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
    // SAFETY(E9): v 为 NUL 结尾宽字符缓冲、存活至 SendMessageW 同步返回；wParam 按 CB_FINDSTRINGEXACT 契约传 -1。
    unsafe {
        SendMessageW!(
            combo,
            CB_FINDSTRINGEXACT,
            WPARAM(0xFFFF_FFFF),
            LPARAM(v.as_ptr() as isize)
        ) as i32
    }
}

// SAFETY(E9): buf 定长 64 元素，GetWindowTextW 按容量截断并返回实际拷贝长度；仅按返回长度切片读取，不越界。
unsafe fn edit_text(hwnd: HWND) -> String {
    let mut buf = [0u16; 64];
    let n = GetWindowTextW(hwnd, &mut buf);
    from_wide_slice(&buf[..n.max(0) as usize])
}

fn set_text(ctx: &Ctx, id: i32, text: &str) {
    let v = wide(text);
    // SAFETY(E9): v 为 NUL 结尾宽字符缓冲、存活至 SetWindowTextW 返回；GetDlgItem 失败得空句柄，由 API 安全忽略。
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
    // SAFETY(E9): hwnd 为调用方持有的有效窗口句柄；GetDlgItem 为纯查询，不产生所有权转移。
    unsafe { GetDlgItem(Some(hwnd), id).unwrap_or_default() }
}

/// 显示/隐藏控件。
// SAFETY(E9): 控件句柄经 ctl() 取自有效父窗口；ShowWindow 仅传值参数。
unsafe fn show_ctl(hwnd: HWND, id: i32, show: bool) {
    let _ = ShowWindow(ctl(hwnd, id), if show { SW_SHOW } else { SW_HIDE });
}

/// 呼出系统取色对话框（CC_FULLOPEN 展开完整色轮 + 自定义色区）。
/// 返回 None = 用户取消。模态期间不持有任何配置借用（WM_ACTIVATE 同步会重入）。
/// v0.3.5 #1（D-52）：COLORREF ↔ #RRGGBB 统一走 config 的唯一转换函数对。
// SAFETY(E9): CHOOSECOLORW 按契约初始化（lStructSize 正确、lpCustColors 指向容量足够的存活缓冲）；模态期间不持有任何配置借用。
unsafe fn pick_color(owner: HWND, initial: RgbColor) -> Option<RgbColor> {
    let mut custom = CUSTOM_COLORS.lock().unwrap_or_else(|e| e.into_inner());
    let mut cc = CHOOSECOLORW {
        lStructSize: std::mem::size_of::<CHOOSECOLORW>() as u32,
        hwndOwner: owner,
        hInstance: HWND::default(),
        rgbResult: COLORREF(rgb_to_colorref(initial)),
        lpCustColors: custom.as_mut_ptr(),
        Flags: CC_RGBINIT | CC_FULLOPEN,
        lCustData: LPARAM(0),
        lpfnHook: None,
        lpTemplateName: PCWSTR::null(),
    };
    if ChooseColorW(&mut cc).as_bool() {
        Some(colorref_to_rgb(cc.rgbResult.0))
    } else {
        None
    }
}

/// 生成 DOT_PX² 的分档色点位图（BGRA，1px 抗锯齿边缘的实心圆点）。
fn tier_dot_bgra(c: RgbColor) -> Vec<u8> {
    let size = DOT_PX;
    let center = (size as f32 - 1.0) / 2.0;
    let radius = (size as f32 / 2.0) - 1.0; // 16px 点留 1px 边距
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

/// 把直 alpha BGRA 打成**预乘 32bpp DIB**（HBITMAP）。SS_BITMAP 控件只对
/// 32bpp DIB 按 alpha 混合；预乘公式 (v*a+127)/255 与图标渲染路径一致。
// SAFETY(E9): bgra 长度由调用方按 w*h*4 保证；CreateDIBSection 位图 bits 写入不越出源切片范围。
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
// SAFETY(E9): dot 为有效 SS_BITMAP 控件句柄；旧位图句柄取回后 DeleteObject，不泄漏、不双删。
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

/// 把颜色画到 swatch 控件（SS_BITMAP，40×24）。
// SAFETY(E9): target 为有效 SS_BITMAP 控件句柄；旧位图句柄取回后 DeleteObject，不泄漏、不双删。
unsafe fn draw_swatch(ctx: &Ctx, hwnd: HWND, id: i32, color: RgbColor) {
    let bgra = swatch_bgra(color, ctx.theme.border, 40, 24);
    let target = ctl(hwnd, id);
    let hbmp = bgra_dib_premultiplied(&bgra, 40, 24);
    if !hbmp.is_invalid() {
        let old = SendMessageW!(
            target,
            STM_SETIMAGE,
            WPARAM(IMAGE_BITMAP.0 as usize),
            LPARAM(hbmp.0 as isize)
        );
        if old != 0 {
            let _ = DeleteObject(HGDIOBJ(old as *mut core::ffi::c_void));
        }
    }
}

/// owner-draw 按钮绘制（D-58：底色 = 按下 > 悬停 > 窗口；1px 边框；居中文本）。
// SAFETY(E9): dis 由 WM_DRAWITEM 传入、系统保证在消息处理期间有效；临时 GDI 对象用毕销毁，选入对象恢复原状。
unsafe fn draw_owner_button(ctx: &Ctx, dis: &DRAWITEMSTRUCT) {
    let t = ctx.theme;
    let pressed = dis.itemState.0 & ODS_SELECTED.0 != 0;
    // 悬停即时判定：光标位于按钮矩形内（父窗口客户坐标）。
    let mut pt = POINT::default();
    let mut hovered = false;
    if GetCursorPos(&mut pt).is_ok() {
        if let Ok(parent) = GetParent(dis.hwndItem) {
            if !parent.0.is_null() {
                let _ = ScreenToClient(parent, &mut pt);
                let r = &dis.rcItem;
                hovered = pt.x >= r.left && pt.x < r.right && pt.y >= r.top && pt.y < r.bottom;
            }
        }
    }
    let bg = if pressed {
        t.pressed
    } else if hovered {
        t.hover
    } else {
        t.window
    };
    let br = CreateSolidBrush(COLORREF(bg));
    FillRect(dis.hDC, &dis.rcItem, br);
    let br_border = ctx.brush(&ctx.br_border);
    if !br_border.is_invalid() {
        FrameRect(dis.hDC, &dis.rcItem, br_border);
    }
    // v0.3.6 F3：焦点态 2px 强调色内描边（规范裁决 6，键盘 Tab 走查必须可见）。
    if dis.itemState.0 & ODS_FOCUS_RAW != 0 {
        let pen = CreatePen(PS_SOLID, 2, COLORREF(t.accent));
        let old_pen = SelectObject(dis.hDC, HGDIOBJ(pen.0));
        let old_br = SelectObject(dis.hDC, GetStockObject(NULL_BRUSH));
        let r = &dis.rcItem;
        let _ = Rectangle(dis.hDC, r.left + 1, r.top + 1, r.right - 1, r.bottom - 1);
        SelectObject(dis.hDC, old_br);
        SelectObject(dis.hDC, old_pen);
        let _ = DeleteObject(HGDIOBJ(pen.0));
    }
    let old = SelectObject(dis.hDC, HGDIOBJ(ctx.font(&ctx.f_base).0));
    let mut buf = [0u16; 64];
    let n = GetWindowTextW(dis.hwndItem, &mut buf);
    let mut txt: Vec<u16> = buf[..n.max(0) as usize].to_vec();
    SetTextColor(dis.hDC, COLORREF(t.text));
    SetBkMode(dis.hDC, TRANSPARENT);
    let mut rc = dis.rcItem;
    DrawTextW(
        dis.hDC,
        &mut txt,
        &mut rc,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
    SelectObject(dis.hDC, old);
    let _ = DeleteObject(HGDIOBJ(br.0));
}

/// 刷新预览与分档提示（v0.3.18：U-2 小预览已删，仅保留放大预览，走 U-5
/// 单一渲染源；U-6 对比度警告条已删）。
fn refresh_preview_and_hints(hwnd: HWND, ctx: &Ctx) {
    let cfg = ctx.cfg.borrow();
    let tiered = cfg.icon_font_tiers_enabled;
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
    // v0.3.18（U-5，单一渲染源）：预览位图直接取自真实托盘图标的渲染
    // 结果——按当前草稿配置临时构建 IconCache，走与任务栏完全相同的
    // IconCache::render_frame 路径、同一位图尺寸（tray_size），不再维护
    // 第二套「相似」实现（旧实现独立调 render_text_bitmap，图3 证实两者
    // 在大字体下结果不一致：预览显示包裹、任务栏实际溢出）。缓存每次
    // 刷新按草稿重建，滑块拖动即时可见（R4 AF-4）；单帧渲染成本与
    // 任务栏同级，可忽略。
    let cache = cache_from_config(&cfg, ctx.tray_size, ctx.dark_theme);
    let rgba = cache.render_frame_rgba(level, &ctx.preview_text);
    let px = ctx.tray_size as i32;

    // 合成到棋盘底（同时示意深浅两种任务栏），再建 HBITMAP。
    let n = (px * px) as usize;
    let mut bgra = vec![0u8; n * 4];
    let cell = (px / 6).max(1);
    for i in 0..n {
        let (py, pxi) = ((i / px as usize) as i32, (i % px as usize) as i32);
        let checker = if ((pxi / cell) + (py / cell)) % 2 == 0 {
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
    // SAFETY(E9): 预览绘制所用 HDC/位图均为本路径创建并于块尾释放；源/目标矩形由控件尺寸推算，不越界读写。
    unsafe {
        // v0.3.18（U-3）：放大预览画布收敛为 144 逻辑像素（96dpi 下恰为 3×）。
        // 放大倍率由画布与源位图尺寸推得（整数最近邻、居中贴入），不写死；
        // 画布四周余量补棋盘格（与源图底色同规格的两档灰）。
        let big = ctx.sc(PREVIEW_BIG_PX);
        // 动态 K 与固定 ×9 在全部 DPI 档数学等价；取动态 K 为单一来源表达，量纲为物理像素。
        let k = (big / px).max(1);
        let scaled = k * px;
        let off = (big - scaled) / 2;
        let cell = (big / 12).max(1);
        let mut big_bgra = vec![0u8; (big * big * 4) as usize];
        for by in 0..big {
            for bx in 0..big {
                let d = ((by * big + bx) * 4) as usize;
                let (ix, iy) = (bx - off, by - off);
                if ix >= 0 && iy >= 0 && ix < scaled && iy < scaled {
                    let s = (((iy / k) * px + (ix / k)) * 4) as usize;
                    big_bgra[d..d + 4].copy_from_slice(&bgra[s..s + 4]);
                } else {
                    let g = if ((bx / cell) + (by / cell)) % 2 == 0 {
                        0xEEu8
                    } else {
                        0x99u8
                    };
                    big_bgra[d] = g;
                    big_bgra[d + 1] = g;
                    big_bgra[d + 2] = g;
                    big_bgra[d + 3] = 255;
                }
            }
        }
        let big_ctl = ctl(hwnd, IDC_PREVIEW_BIG);
        let hbmp2 = bgra_dib_premultiplied(&big_bgra, big, big);
        if !hbmp2.is_invalid() {
            let old = SendMessageW!(
                big_ctl,
                STM_SETIMAGE,
                WPARAM(IMAGE_BITMAP.0 as usize),
                LPARAM(hbmp2.0 as isize)
            );
            if old != 0 {
                let _ = DeleteObject(HGDIOBJ(old as *mut core::ffi::c_void));
            }
        }
    }

    // v0.3.18（U-6）：对比度警告条删除——任务栏即真实效果，透明背景下
    // 软件无从判断任务栏真实颜色，警告既不可靠也与「恢复全部默认」按钮
    // 视觉重叠；用户可直接看任务栏判断可读性。
    // v0.3.6：分档说明示例随当前档数/阈值实时拼写（替代静态常量）。
    let tier_hint = tier_hint_dynamic(&cfg.icon_font_tier_thresholds);
    let lab_enabled = cfg.experimental_enabled_count();
    drop(cfg);
    set_text(ctx, IDC_TIER_HINT, &tier_hint);
    // v0.3.22（D-87/D-88）：闸门计数常驻动态显示 + hint 动态「已修改」判定。
    set_text(ctx, IDC_FINE_TUNE_GATE, &fine_tune_gate_text(lab_enabled));
    set_text(ctx, IDC_EXPERIMENTAL_HINT, &lab_hint_text(lab_dirty(ctx)));
}

/// 程序化同步全部控件 ↔ 工作配置（抑制自身事件）。
fn sync_controls(hwnd: HWND, ctx: &Ctx) {
    ctx.syncing.set(true);
    let cfg = ctx.cfg.borrow();
    // SAFETY(E9): 全部控件句柄经 ctl() 取自有效设置窗口；cfg 借用限于块内读取，无重入修改。
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
            IconBackground::Custom(_) => combo_set_sel(bg, 2),
        }
        let (bg_r, bg_g, bg_b) = cfg.icon_background.rgb();
        draw_swatch(
            ctx,
            hwnd,
            IDC_BG_SWATCH,
            RgbColor {
                r: bg_r,
                g: bg_g,
                b: bg_b,
            },
        );

        // 裁决 1：下拉选择即分档开关。自动 → 展开分档卡；自定义 → 单一选色。
        let tiered = cfg.icon_font_tiers_enabled;
        let fc = ctl(hwnd, IDC_FONT_COLOR);
        combo_set_sel(fc, if tiered { 0 } else { 1 });
        let font_color = match cfg.icon_font_color {
            IconFontColor::Fixed(c) => c,
            IconFontColor::Auto => RgbColor {
                r: 0xFF,
                g: 0xFF,
                b: 0xFF,
            },
        };
        draw_swatch(ctx, hwnd, IDC_FONT_SWATCH, font_color);
        show_ctl(hwnd, IDC_FONT_SWATCH, !tiered);
        show_ctl(hwnd, IDC_FONT_PICK, !tiered);
        // v0.3.6：跟踪已选自定义色（热重载/重置落地 Fixed 时同步记忆）。
        if let IconFontColor::Fixed(c) = cfg.icon_font_color {
            ctx.last_custom_font.set(c);
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
        // 间隔：取 ≤ 当前值的最大档；低于最低档取最低档（D-66）。
        let iv = ctl(hwnd, IDC_INTERVAL);
        let idx = POLL_TIERS_SECS
            .iter()
            .rposition(|s| *s <= cfg.poll_interval_secs)
            .unwrap_or(0);
        combo_set_sel(iv, idx as i32);

        // —— 分档卡（裁决 2）——
        let n = cfg
            .icon_font_tier_colors
            .len()
            .clamp(FONT_TIER_MIN_TIERS, FONT_TIER_MAX_TIERS);
        combo_set_sel(ctl(hwnd, IDC_TIER_COUNT), (n - FONT_TIER_MIN_TIERS) as i32);
        let ths = cfg.icon_font_tier_thresholds.clone();
        for i in 0..4usize {
            let e = ctl(hwnd, IDC_TIER_TH0 + i as i32);
            if let Some(t) = ths.get(i) {
                let v = wide(&t.to_string());
                let _ = SetWindowTextW(e, PCWSTR(v.as_ptr()));
            }
        }
        // v0.3.6 F2：程序化同步清空阈值错误态（非法值从未落地，同步即合法态）。
        if ctx.tier_err.borrow().iter().any(|e| *e) {
            *ctx.tier_err.borrow_mut() = [false; 4];
            let _ = InvalidateRect(Some(hwnd), None, true);
        }
        set_text(ctx, IDC_TIER_ERR, "");
        let cols = cfg.icon_font_tier_colors.clone();
        let tip = HWND(ctx.tooltip.get() as *mut core::ffi::c_void);
        for i in 0..5usize {
            let d = IDC_TIER_DOT0 + i as i32;
            if let Some(c) = cols.get(i) {
                draw_dot(hwnd, d, *c);
            }
            // tooltip 文案：档位区间 + 当前色 HEX（如「21–50 · #FFB900」）。
            let lo = if i == 0 {
                0
            } else {
                ths.get(i - 1).copied().unwrap_or(0) as i32 + 1
            };
            let hi = match ths.get(i) {
                Some(t) => *t as i32,
                None => 100,
            };
            let text = format!(
                "{lo}–{hi} · {}",
                cols.get(i).map(|c| c.to_hex()).unwrap_or_default()
            );
            tip_tool(tip, hwnd, ctl(hwnd, d), &text, false);
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
        // v0.3.22（D-87）：闸门与实验室四项复选框回填（程序化同步，抑制事件）。
        SendMessageW!(
            ctl(hwnd, IDC_FINE_TUNE_GATE),
            BM_SETCHECK,
            WPARAM(if cfg.ui_fine_tune_expanded {
                BST_CHECKED.0 as usize
            } else {
                BST_UNCHECKED.0 as usize
            }),
            LPARAM(0)
        );
        for (id, _, _) in LAB_ITEMS.iter() {
            let on = match *id {
                IDC_LAB_PERSIST => cfg.experimental_persist_last_known,
                IDC_LAB_STATS => cfg.experimental_stats,
                IDC_LAB_POLLING => cfg.experimental_per_device_polling,
                IDC_LAB_BT => cfg.experimental_bluetooth_wired,
                _ => false,
            };
            SendMessageW!(
                ctl(hwnd, *id),
                BM_SETCHECK,
                WPARAM(if on {
                    BST_CHECKED.0 as usize
                } else {
                    BST_UNCHECKED.0 as usize
                }),
                LPARAM(0)
            );
        }
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
        // 文本/背景覆盖度滑块回填（D-57）。
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

        // v0.3.22（D-87）：闸门展开态变化才重排（窗口高度随实验室分组
        // 增减 EXP_GATE_DELTA；与分档卡同一 relayout 路径）。
        let gate = cfg.ui_fine_tune_expanded;
        if ctx.gate_expanded.get() != gate {
            ctx.gate_expanded.set(gate);
            relayout(hwnd, ctx, tiered);
        } else if ctx.expanded.get() != tiered {
            // 展开态变化才重排（收起回收 112px，展开还原）。
            relayout(hwnd, ctx, tiered);
        }
        // 档内二次可见性：N 档 = N-1 个阈值框 + N 个色点。
        if tiered {
            for i in 0..4usize {
                show_ctl(hwnd, IDC_TIER_TH0 + i as i32, i < n - 1);
            }
            for i in 0..5usize {
                show_ctl(hwnd, IDC_TIER_DOT0 + i as i32, i < n);
            }
        }
    }
    ctx.syncing.set(false);
    refresh_preview_and_hints(hwnd, ctx);
}

/// 「选色…」按钮与分档色点点击：先取初始色（短借用）→ 呼出模态取色 → 回写。
// SAFETY(E9): 初始色在短借用内取出后即释放 cfg 借用再进模态取色；回写重新借用，不跨模态持有引用。
unsafe fn handle_color_pick(hwnd: HWND, ctx: &Ctx, id: i32) {
    let initial = {
        let cfg = ctx.cfg.borrow();
        match id {
            IDC_BG_PICK => {
                let (r, g, b) = cfg.icon_background.rgb();
                RgbColor { r, g, b }
            }
            IDC_FONT_PICK => match cfg.icon_font_color {
                IconFontColor::Fixed(c) => c,
                // v0.3.6：取色初始值用用户上次所选自定义色（不再一律纯白）。
                IconFontColor::Auto => ctx.last_custom_font.get(),
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
            }
            IDC_FONT_PICK => {
                cfg.icon_font_color = IconFontColor::Fixed(picked);
                // v0.3.6：记住用户已选自定义色（自动→自定义→自动往返不丢失）。
                ctx.last_custom_font.set(picked);
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

/// 阈值非法反馈（v0.3.6 F2，规范裁决 2）：错误位 + 分档卡内右侧红色提示 +
/// 触发 1px 错误色描边重绘；全程不占底部状态行（避免被对比度提示覆写）。
fn set_tier_error(hwnd: HWND, ctx: &Ctx, i: usize, msg: Option<&str>) {
    {
        let mut errs = ctx.tier_err.borrow_mut();
        if i >= errs.len() {
            return;
        }
        errs[i] = msg.is_some();
    }
    set_text(ctx, IDC_TIER_ERR, msg.unwrap_or(""));
    // SAFETY(E9): hwnd 为有效设置窗口句柄；InvalidateRect 仅传值参数。
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, true);
    }
}

fn on_command(hwnd: HWND, ctx: &Ctx, id: i32, code: u32) {
    if ctx.syncing.get() {
        return;
    }
    let clicked = code == BN_CLICKED;
    // 取色按钮 / 分档色点（STN_CLICKED 与 BN_CLICKED 同为 0）。
    if clicked
        && (id == IDC_BG_PICK
            || id == IDC_FONT_PICK
            || (IDC_TIER_DOT0..IDC_TIER_DOT0 + 5).contains(&id))
    {
        // SAFETY(E9): 转调 handle_color_pick，其不安全不变量见该函数注释；此处传入的窗口句柄与控件 id 均有效。
        unsafe { handle_color_pick(hwnd, ctx, id) };
        return;
    }
    // v0.3.22（D-88）：立即重启——复用既有启动路径 spawn 当前 exe，由主
    // 循环统一优雅退出；排程代码零改动、非热生效（不在此处直接重启）。
    if clicked && id == IDC_LAB_RESTART {
        crate::request_restart();
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
                    // 「自定义」保留既有自定义色；首次切换给默认深灰。
                    cfg.icon_background = match cfg.icon_background {
                        IconBackground::Custom(c) => IconBackground::Custom(c),
                        _ => IconBackground::Custom(RgbColor {
                            r: 0x1F,
                            g: 0x1F,
                            b: 0x1F,
                        }),
                    };
                }
                _ => return,
            }
            touched = true;
            resync = true; // swatch 刷新
        }
        IDC_FONT_FAMILY if selchange => {
            let name = combo_current_text(ctl(hwnd, IDC_FONT_FAMILY));
            if name.is_empty() {
                return;
            }
            cfg.icon_font_family = name;
            touched = true;
        }
        // 裁决 1：下拉选择即模式开关（icon_font_tiers_enabled 由下拉派生）。
        IDC_FONT_COLOR if selchange => {
            match combo_sel(ctl(hwnd, IDC_FONT_COLOR)) {
                0 => {
                    // v0.3.6：切「自动」前记住已选自定义色（往返不丢，P3 场景瑕疵①）。
                    if let IconFontColor::Fixed(c) = cfg.icon_font_color {
                        ctx.last_custom_font.set(c);
                    }
                    cfg.icon_font_tiers_enabled = true;
                    cfg.icon_font_color = IconFontColor::Auto;
                }
                1 => {
                    cfg.icon_font_tiers_enabled = false;
                    // v0.3.6：恢复用户上次所选自定义色（不再回填纯白）。
                    cfg.icon_font_color = IconFontColor::Fixed(ctx.last_custom_font.get());
                }
                _ => return,
            }
            touched = true;
            resync = true; // 分档卡展开/收起
        }
        // 档数调整：增长时在 [上一档+1, 99] 区间均分剩余档位，再过 normalize。
        IDC_TIER_COUNT if selchange => {
            let idx = combo_sel(ctl(hwnd, IDC_TIER_COUNT));
            if idx < 0 {
                return;
            }
            let n = FONT_TIER_MIN_TIERS + idx as usize;
            // E23 第 2 步：均分/弹尾逻辑逐字抽离到 app::settings_logic
            // （末档 99 时的 clamp panic 潜伏缺陷已于小修批第 1 项修复，
            // 见该函数注释）。
            let ths = retier_thresholds(&cfg.icon_font_tier_thresholds, n);
            let (t2, c2, _) = normalize_font_tiers(ths, cfg.icon_font_tier_colors.clone());
            cfg.icon_font_tier_thresholds = t2;
            cfg.icon_font_tier_colors = c2;
            touched = true;
            resync = true;
        }
        // 阈值数字框：1–99、严格递增，非法即时提示且不落地（v0.3.6 F2：错误描边 +
        // 卡内右侧红色提示，不占状态行；即时生效模式无确定按钮可禁用，见 D-60）。
        id if change && (IDC_TIER_TH0..IDC_TIER_TH0 + 4).contains(&id) => {
            let i = (id - IDC_TIER_TH0) as usize;
            // SAFETY(E9): 转调 edit_text（不变量见其注释：定长缓冲截断读取）；控件句柄经 ctl() 取自有效父窗口。
            let text = unsafe { edit_text(ctl(hwnd, id)) };
            if text.is_empty() {
                set_tier_error(hwnd, ctx, i, None); // 输入中间态
                return;
            }
            // E23 第 2 步：解析与区间校验抽离到 app::settings_logic（文案常量同源）。
            let v: u8 = match parse_tier_threshold(&text) {
                Ok(v) => v,
                Err(msg) => {
                    set_tier_error(hwnd, ctx, i, Some(msg));
                    return;
                }
            };
            if i >= cfg.icon_font_tier_thresholds.len() {
                return;
            }
            let mut ths = cfg.icon_font_tier_thresholds.clone();
            ths[i] = v;
            if !thresholds_strictly_increasing(&ths) {
                set_tier_error(hwnd, ctx, i, Some(TIER_THRESHOLD_ERR_NOT_INCREASING));
                return;
            }
            cfg.icon_font_tier_thresholds = ths;
            set_tier_error(hwnd, ctx, i, None);
            touched = true;
        }
        IDC_INTERVAL if selchange => {
            let idx = combo_sel(ctl(hwnd, IDC_INTERVAL));
            if idx >= 0 && (idx as usize) < POLL_TIERS_SECS.len() {
                cfg.poll_interval_secs = POLL_TIERS_SECS[idx as usize];
                touched = true;
            }
        }
        IDC_BLINK if clicked => {
            let checked =
                // SAFETY(E9): 控件句柄经 ctl() 取自有效窗口；BM_GETCHECK 仅传值参数，返回值与 BST_CHECKED 比较。
                unsafe { SendMessageW!(ctl(hwnd, IDC_BLINK), BM_GETCHECK, WPARAM(0), LPARAM(0)) };
            cfg.blink_enabled = checked == BST_CHECKED.0 as isize;
            touched = true;
        }
        // 行内重置（规范默认：底行 + 就地）。
        IDC_TEXT_RESET if clicked => {
            // v0.3.18（U-1）：默认档随新基准 140（不再硬编码 100）。
            cfg.icon_text_scale = ICON_TEXT_SCALE_DEFAULT;
            touched = true;
            resync = true;
        }
        IDC_BG_RESET if clicked => {
            cfg.icon_bg_scale = 100;
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
            // 注册表自启态与默认值对齐，避免 TOML 与 HKCU Run 键分叉。
            let exe =
                std::env::current_exe().unwrap_or_else(|_| PathBuf::from("volttray-tray.exe"));
            let _ = autostart::sync(cfg.autostart, &exe);
            touched = true;
            resync = true;
        }
        // v0.3.22（D-87）：精细化设置闸门——只管可见性，不管使能（收起≠关闭）。
        // 注意：这里不直接 set ctx.gate_expanded——留给 sync_controls 的
        // 「闸门态变化才重排」分支检测并调 relayout（窗口高度随实验室
        // 分组增减 EXP_GATE_DELTA，同一 relayout 路径），否则重排永不触发。
        IDC_FINE_TUNE_GATE if clicked => {
            // SAFETY(E9): 控件句柄经 ctl() 取自有效窗口；BM_GETCHECK 仅传值参数，返回值与 BST_CHECKED 比较。
            let checked = unsafe {
                SendMessageW!(
                    ctl(hwnd, IDC_FINE_TUNE_GATE),
                    BM_GETCHECK,
                    WPARAM(0),
                    LPARAM(0)
                )
            };
            cfg.ui_fine_tune_expanded = checked == BST_CHECKED.0 as isize;
            touched = true;
            resync = true;
        }
        // v0.3.22（D-88）：四项试验功能开关——保存后重启生效（非热生效）。
        IDC_LAB_PERSIST if clicked => {
            // SAFETY(E9): 控件句柄经 ctl() 取自有效窗口；BM_GETCHECK 仅传值参数，返回值与 BST_CHECKED 比较。
            let checked = unsafe {
                SendMessageW!(
                    ctl(hwnd, IDC_LAB_PERSIST),
                    BM_GETCHECK,
                    WPARAM(0),
                    LPARAM(0)
                )
            };
            cfg.experimental_persist_last_known = checked == BST_CHECKED.0 as isize;
            touched = true;
        }
        IDC_LAB_STATS if clicked => {
            // SAFETY(E9): 控件句柄经 ctl() 取自有效窗口；BM_GETCHECK 仅传值参数，返回值与 BST_CHECKED 比较。
            let checked = unsafe {
                SendMessageW!(ctl(hwnd, IDC_LAB_STATS), BM_GETCHECK, WPARAM(0), LPARAM(0))
            };
            cfg.experimental_stats = checked == BST_CHECKED.0 as isize;
            touched = true;
        }
        IDC_LAB_POLLING if clicked => {
            // SAFETY(E9): 控件句柄经 ctl() 取自有效窗口；BM_GETCHECK 仅传值参数，返回值与 BST_CHECKED 比较。
            let checked = unsafe {
                SendMessageW!(
                    ctl(hwnd, IDC_LAB_POLLING),
                    BM_GETCHECK,
                    WPARAM(0),
                    LPARAM(0)
                )
            };
            cfg.experimental_per_device_polling = checked == BST_CHECKED.0 as isize;
            touched = true;
        }
        IDC_LAB_BT if clicked => {
            let checked =
                // SAFETY(E9): 控件句柄经 ctl() 取自有效窗口；BM_GETCHECK 仅传值参数，返回值与 BST_CHECKED 比较。
                unsafe { SendMessageW!(ctl(hwnd, IDC_LAB_BT), BM_GETCHECK, WPARAM(0), LPARAM(0)) };
            cfg.experimental_bluetooth_wired = checked == BST_CHECKED.0 as isize;
            touched = true;
        }
        // v0.3.22（D-86）：一键对齐——仅四项置为默认开启，不动其余配置。
        IDC_LAB_RESET if clicked => {
            cfg.experimental_persist_last_known = true;
            cfg.experimental_stats = true;
            cfg.experimental_per_device_polling = true;
            cfg.experimental_bluetooth_wired = true;
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
