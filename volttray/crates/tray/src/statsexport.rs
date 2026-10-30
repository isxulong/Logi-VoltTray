//! WP-B 耗电统计导出的 GUI 胶水层（v3 唯一基线）：
//! - 「导出耗电统计…」编排：选项对话框（可视化报告 / 匿名化设备键 /
//!   JSON 导出三开关）→ 复用 v0.3.15 Save-As（CSV 过滤器、秒级命名
//!   R5-P-02）→ IoCmd::ExportStats 派给 IO 线程（导出前先 flush，
//!   渲染/写盘在后台线程执行、不阻塞 UI，v3 已知限制 P2-③ 口径）。
//! - I-02（R5 终裁 P1）：WM_QUERYENDSESSION / WM_ENDSESSION 承接窗
//!   口——新建常驻隐藏**顶层**窗口（现有 Raw Input 窗口为
//!   HWND_MESSAGE message-only，按 Win32 语义不接收广播消息，无法
//!   承接 WM_ENDSESSION）；收到真实关机广播即通知 IO 线程执行
//!   run_stop + 双 store shutdown_flush，并最多等待 2.5s 落盘完成，
//!   关机 flush 路径不再静默失效。

use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, SIZE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateFontIndirectW, DeleteObject, EnumFontFamiliesExW, GetDC, GetStockObject,
    GetTextExtentPoint32W, ReleaseDC, SelectObject, DEFAULT_CHARSET, DEFAULT_GUI_FONT, HFONT,
    HGDIOBJ, LOGFONTW, TEXTMETRICW,
};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::Controls::BST_CHECKED;
use windows::Win32::UI::WindowsAndMessaging::*;

use volttray_app::diag_export::LocalTimeParts;

use crate::IoCmd;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 导出请求（GUI → IO 线程；渲染/写盘在 IO 线程执行）。
pub struct StatsExportRequest {
    pub csv_path: PathBuf,
    pub html: bool,
    pub anonymize: bool,
    pub json: bool,
    /// 融合稿 Phase 1：导出内容是否包含静止/活动分钟统计
    /// （activity_buckets 事件；默认开）。
    pub include_activity: bool,
    /// 组5 5b（C-2）：anonymize=on 时 display_name → 「设备N」
    ///（R3 会话映射）；false = 回退保留真实型号。
    pub redact_display_name: bool,
    /// 组5 5b（U-2 闸门，BvA-5 默认关）：true → CSV/HTML 时间戳
    /// 按 UTC（+00:00）渲染；false → 本地时区（现状）。
    pub tz_utc: bool,
    /// 组4 N-08②：逐分钟活动位图独立开关（默认关，与 anonymize
    /// 解耦——作息泄露与是否匿名无关；对话框勾选文案明示
    /// 「含作息信息」）。false → 三件套不含 activity_buckets
    ///（BvA-8：与 manifest schema_version 同批合入，下游按
    /// schema_version 判读、禁止按字段存在性硬解析）。
    pub export_activity_bitmap: bool,
}

/// 组5 5b 导出脱敏/知情闸门（main 从配置读取注入；C-2 回滚键默认
/// 开，BvA-5 时区归一默认关）。privacy_notice 只作用于 GUI 线程
/// 的保存前提示，不随请求进 IO 线程。
#[derive(Debug, Clone, Copy)]
pub struct ExportGates {
    /// anonymize=on 时 display_name 映射「设备N」（R3）；false =
    /// 回退保留真实型号（强制提示「脱敏修复已回退」）。
    pub redact_display_name: bool,
    /// 导出时间统一 UTC（U-2 闸门，BvA-5 默认关）。
    pub tz_utc: bool,
    /// 保存前知情提示「本文件含设备型号与作息信息」（U-2C）；
    /// false = 回退不提示（强制提示「脱敏修复已回退」）。
    pub privacy_notice: bool,
}

impl Default for ExportGates {
    fn default() -> Self {
        Self {
            redact_display_name: true,
            tz_utc: false,
            privacy_notice: true,
        }
    }
}

/// 组5 5b：模态知情提示（U-2C 保存前提示 / C-2 回退提示共用）。
fn notice(title: &str, text: &str) {
    let t = wide(title);
    let b = wide(text);
    // SAFETY(E9): 宽字符缓冲 NUL 结尾、调用期间存活；MessageBoxW 同步模态，无指针逃逸。
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(b.as_ptr()),
            PCWSTR(t.as_ptr()),
            MB_OK | MB_ICONINFORMATION,
        );
    }
}

/// 导出选项（对话框五开关；默认：HTML 开、匿名化关、JSON 关、
/// 含活动统计开、逐分钟活动位图关——physical_key 默认保留（本机
/// 自用口径，v3 文档标注敏感级），HTML 报告为本期拍板特性默认随
/// 导出生成；含活动统计默认开（融合稿 §6 statsexport.rs 行）；
/// 位图默认关（组4 N-08②：作息泄露面独立收口，与匿名化解耦）。
#[derive(Debug, Clone, Copy)]
pub struct ExportOptions {
    pub html: bool,
    pub anonymize: bool,
    pub json: bool,
    pub include_activity: bool,
    pub export_activity_bitmap: bool,
}

// —— R-5/R-6（v0.3.17 真机终审修复）：UI 字体回退栈 + 复选框宽度实测 ——
/// R-5：中文 UI 字体回退栈（逐控件 WM_SETFONT）。真机简体 Windows 上
/// 默认 MS Shell Dlg 映射缺中文字形导致对话框乱码（终审现象 4）：
/// 按序探测系统实际安装的字体，首个可用者生效；三选皆缺退回
/// DEFAULT_GUI_FONT stock 对象。
const UI_FONT_CANDIDATES: [&str; 3] = ["Microsoft YaHei UI", "Segoe UI", "MS Shell Dlg 2"];

/// R-6：复选框系统字形占位宽（勾选框方块 + 内边距，逻辑像素）。
const CHECKBOX_GLYPH_W: i32 = 22;
/// R-6：对话框左 / 右留白（逻辑像素）。
const DLG_SIDE_MARGIN: i32 = 16;
const DLG_RIGHT_MARGIN: i32 = 24;

/// CreateFontIndirectW 对不存在的字面值不会失败（字体映射器静默
/// 替换），字体是否存在必须用 EnumFontFamiliesExW 实测。探测本身
/// 失败（拿不到 DC 等）按「存在」处理，避免误杀合法字体。
fn font_available(face: &str) -> bool {
    struct Hit(bool);
    // SAFETY(E9): 字体枚举回调：LOGFONTW 指针由系统在回调期间保证有效、仅读取；lparam 还原的 Hit 指针由调用方保证存活至枚举结束。
    unsafe extern "system" fn enum_proc(
        _lpelf: *const LOGFONTW,
        _lpntme: *const TEXTMETRICW,
        _ft: u32,
        lparam: LPARAM,
    ) -> i32 {
        (*(lparam.0 as *mut Hit)).0 = true;
        0
    }
    let face_w = wide(face);
    let mut lf = LOGFONTW {
        lfCharSet: DEFAULT_CHARSET,
        ..Default::default()
    };
    let n = (face_w.len() - 1).min(lf.lfFaceName.len() - 1);
    lf.lfFaceName[..n].copy_from_slice(&face_w[..n]);
    let mut hit = Hit(false);
    // SAFETY(E9): GetDC(None) 取得的 DC 用毕 ReleaseDC；LOGFONTW 按值构造、字体名保持 NUL 结尾。
    unsafe {
        let hdc = GetDC(None);
        if hdc.is_invalid() {
            return true;
        }
        EnumFontFamiliesExW(
            hdc,
            &lf,
            Some(enum_proc),
            LPARAM(&mut hit as *mut Hit as isize),
            0,
        );
        let _ = ReleaseDC(None, hdc);
    }
    hit.0
}

/// 创建 UI 字体：回退栈中首个实际存在的字体；三选皆缺退回
/// DEFAULT_GUI_FONT（stock 对象，返回 owned=false，不得 DeleteObject）。
fn create_ui_font() -> (HFONT, bool) {
    for face in UI_FONT_CANDIDATES {
        if font_available(face) {
            let face_w = wide(face);
            let mut lf = LOGFONTW {
                lfHeight: -12,
                lfWeight: 400,
                lfCharSet: DEFAULT_CHARSET,
                ..Default::default()
            };
            let n = (face_w.len() - 1).min(lf.lfFaceName.len() - 1);
            lf.lfFaceName[..n].copy_from_slice(&face_w[..n]);
            // SAFETY(E9): lf 按值构造；CreateFontIndirectW 返回无效句柄时不使用，有效句柄移交调用方持有。
            let font = unsafe { CreateFontIndirectW(&lf) };
            if !font.is_invalid() {
                return (font, true);
            }
        }
    }
    // SAFETY(E9): GetStockObject 返回系统库存 GDI 对象句柄，不销毁、不转移所有权。
    let stock = unsafe { GetStockObject(DEFAULT_GUI_FONT) };
    (HFONT(stock.0), false)
}

/// R-6：实测文本像素宽（复选框行宽与窗口宽据此运行时计算，不硬编码）。
/// 拿不到 DC 时按每字符 8px 兜底估算。
fn measure_text_width(text: &str, font: HFONT) -> i32 {
    let text_w = wide(text);
    let text_w = &text_w[..text_w.len() - 1];
    // SAFETY(E9): GetDC/ReleaseDC 配对；SelectObject 选入字体后恢复原对象；文本长度显式传入。
    unsafe {
        let hdc = GetDC(None);
        if hdc.is_invalid() {
            return text_w.len() as i32 * 8;
        }
        let old = SelectObject(hdc, HGDIOBJ(font.0));
        let mut size = SIZE::default();
        let _ = GetTextExtentPoint32W(hdc, text_w, &mut size);
        SelectObject(hdc, old);
        let _ = ReleaseDC(None, hdc);
        size.cx
    }
}

const IDC_HTML: u32 = 1001;
const IDC_ANON: u32 = 1002;
const IDC_JSON: u32 = 1003;
const IDC_ACTIVITY: u32 = 1004;
const IDC_BITMAP: u32 = 1005;
const ID_EXPORT_OK: u32 = 1101;
const ID_EXPORT_CANCEL: u32 = 1102;

struct DialogState {
    result: Option<ExportOptions>,
}

// SAFETY(E9): 对话框过程回调：用户数据指针在 WM_INITDIALOG 建立、对话框销毁前有效；其余消息先判空再解引用。
unsafe extern "system" fn export_dlg_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as u32;
            if id == ID_EXPORT_OK || id == ID_EXPORT_CANCEL {
                let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut DialogState;
                if !state_ptr.is_null() && id == ID_EXPORT_OK {
                    let check = |cid: u32| {
                        SendMessageW(
                            GetDlgItem(Some(hwnd), cid as i32).unwrap_or_default(),
                            BM_GETCHECK,
                            None,
                            None,
                        )
                        .0 == BST_CHECKED.0 as isize
                    };
                    (*state_ptr).result = Some(ExportOptions {
                        html: check(IDC_HTML),
                        anonymize: check(IDC_ANON),
                        json: check(IDC_JSON),
                        include_activity: check(IDC_ACTIVITY),
                        export_activity_bitmap: check(IDC_BITMAP),
                    });
                }
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 导出选项对话框（模态）：五个勾选 + 确定/取消。取消返回 None
/// （零落盘、零派发）。进度提示（v3 P2-③）：对话框内静态文案告知
/// 「后台执行、完成弹窗」，渲染期间 UI 不被阻塞（派往 IO 线程）。
pub fn pick_export_options() -> Option<ExportOptions> {
    // SAFETY(E9): 对话框创建按 Win32 契约（模块句柄/资源模板有效）；控件句柄均为本对话框有效子窗口；失败路径返回 None 不留半成品。
    unsafe {
        let hinst = match windows::Win32::System::LibraryLoader::GetModuleHandleW(None) {
            Ok(h) => windows::Win32::Foundation::HINSTANCE(h.0),
            Err(_) => return None,
        };
        let class = wide("VoltTrayStatsExportWnd");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(export_dlg_wndproc),
            hInstance: hinst,
            lpszClassName: PCWSTR(class.as_ptr()),
            hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(
                (windows::Win32::Graphics::Gdi::COLOR_WINDOW.0 + 1) as *mut core::ffi::c_void,
            ),
            ..Default::default()
        };
        let atom = RegisterClassW(&wc);
        if atom == 0 {
            let err = windows::Win32::Foundation::GetLastError();
            if err != windows::Win32::Foundation::ERROR_CLASS_ALREADY_EXISTS {
                crate::bootlog::note(&format!(
                    "stats export dialog: RegisterClassW failed: {err:?}"
                ));
                return None;
            }
        }
        let mut state = DialogState { result: None };
        let title = wide("导出耗电统计");
        // R-5：UI 字体回退栈（创建一次，逐控件 WM_SETFONT）。
        let (ui_font, font_owned) = create_ui_font();
        // R-6：复选框行宽 = 实测文本宽 + 20% 余量 + 勾选框字形占位；
        // 窗口宽由最宽行运行时计算（不硬编码），下限 200。
        let cb_texts = [
            "同时生成可视化报告（HTML，同名主干 .html）",
            "匿名化设备键（device_key 显示为 D1、D2…）",
            "同时导出原始事件序列（JSON，同名主干 .json）",
            "含活动统计（静止/活动摘要，随事件流并入）",
            "逐分钟活动位图（含作息信息）",
        ];
        let widest = cb_texts
            .iter()
            .map(|t| measure_text_width(t, ui_font))
            .max()
            .unwrap_or(0);
        let cb_w = widest + widest / 5 + CHECKBOX_GLYPH_W;
        let win_w = (DLG_SIDE_MARGIN + cb_w + DLG_RIGHT_MARGIN).max(200);
        let x = (GetSystemMetrics(SM_CXSCREEN) - win_w) / 2;
        let y = (GetSystemMetrics(SM_CYSCREEN) - 292) / 2;
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            x.max(0),
            y.max(0),
            win_w,
            292,
            None,
            None,
            Some(hinst),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                crate::bootlog::note(&format!("stats export dialog: CreateWindowExW failed: {e}"));
                return None;
            }
        };
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, &mut state as *mut DialogState as isize);
        // 控件：说明 + 五勾选 + 确定/取消（子窗口随父窗口销毁，无需显式释放）。
        // R-5：每个控件创建成功后逐控件 WM_SETFONT（lParam=1 立即重绘）。
        let mk = |cls: &str,
                  text: &str,
                  style: WINDOW_STYLE,
                  x: i32,
                  y: i32,
                  w: i32,
                  h: i32,
                  id: u32| {
            let cls_w = wide(cls);
            let text_w = wide(text);
            let r = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(cls_w.as_ptr()),
                PCWSTR(text_w.as_ptr()),
                style | WS_CHILD | WS_VISIBLE,
                x,
                y,
                w,
                h,
                Some(hwnd),
                Some(windows::Win32::UI::WindowsAndMessaging::HMENU(
                    id as *mut core::ffi::c_void,
                )),
                Some(hinst),
                None,
            );
            if let Ok(h) = &r {
                SendMessageW(
                    *h,
                    WM_SETFONT,
                    Some(WPARAM(ui_font.0 as usize)),
                    Some(LPARAM(1)),
                );
            }
            r
        };
        let _ = mk(
            "STATIC",
            "选择导出内容。导出在后台执行，完成后弹窗提示结果。",
            WINDOW_STYLE(0),
            12,
            12,
            win_w - 24,
            32,
            0,
        );
        // R-6：五个复选框共用实测行宽（文本数组与上面宽度实测同一份）。
        let mut h_html = HWND::default();
        let mut h_activity = HWND::default();
        for (i, text) in cb_texts.iter().enumerate() {
            let id = [IDC_HTML, IDC_ANON, IDC_JSON, IDC_ACTIVITY, IDC_BITMAP][i];
            let h = mk(
                "BUTTON",
                text,
                WS_GROUP | WS_TABSTOP | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
                DLG_SIDE_MARGIN,
                52 + i as i32 * 26,
                cb_w,
                20,
                id,
            )
            .unwrap_or_default();
            if id == IDC_HTML {
                h_html = h;
            }
            if id == IDC_ACTIVITY {
                h_activity = h;
            }
        }
        // R-6：按钮随窗口宽居中（不再按 360 宽硬编码）。
        let _ = mk(
            "BUTTON",
            "选择保存位置…",
            WS_GROUP | WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
            win_w / 2 - 120,
            202,
            110,
            28,
            ID_EXPORT_OK,
        );
        let _ = mk(
            "BUTTON",
            "取消",
            WS_GROUP | WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32),
            win_w / 2 + 10,
            202,
            110,
            28,
            ID_EXPORT_CANCEL,
        );
        // 默认勾选 HTML 报告（本期拍板特性）与含活动统计（融合稿
        // Phase 1：「含活动统计」勾选项默认开）；逐分钟活动位图
        // 默认不勾（组4 N-08②：作息泄露面默认收口，用户显式勾选
        // 才导出，文案已明示「含作息信息」）。
        for h in [h_html, h_activity] {
            SendMessageW(h, BM_SETCHECK, Some(WPARAM(BST_CHECKED.0 as usize)), None);
        }
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = windows::Win32::Graphics::Gdi::UpdateWindow(hwnd);
        // 模态循环：对话框销毁时 PostQuitMessage 退出本循环。
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        // R-5：自有字体对象随对话框关闭释放（stock 对象不删）。
        if font_owned {
            let _ = DeleteObject(HGDIOBJ(ui_font.0));
        }
        state.result
    }
}

/// 「导出耗电统计…」菜单编排：选项对话框 →（组5 5b：回退强制提示 /
/// 保存前知情提示）→ 秒级默认命名（R5-P-02）→ Save-As（CSV 过滤器）
/// → 派发 IO 线程。任一步取消即零落盘零派发。
pub fn begin_export(cmd_tx: &mpsc::Sender<IoCmd>, gates: ExportGates) {
    let Some(opts) = pick_export_options() else {
        crate::bootlog::note("stats export cancelled at options dialog");
        return;
    };
    // C-2（组5 5b）：回滚键（默认开的脱敏修复项）被关闭时强制提示
    // 「脱敏修复已回退」——不可跳过、不挂开关。
    if !gates.redact_display_name || !gates.privacy_notice {
        let mut items: Vec<&str> = Vec::new();
        if !gates.redact_display_name {
            items.push("· 设备型号匿名化（导出将保留真实型号）");
        }
        if !gates.privacy_notice {
            items.push("· 保存前知情提示（本次起不再提示）");
        }
        notice(
            "脱敏修复已回退",
            &format!(
                "以下脱敏修复项已通过配置关闭，导出将按回退行为执行：\n{}\n\
                 如需恢复，请将配置中对应 experimental_export_* 键改回 true 并重启。",
                items.join("\n")
            ),
        );
    }
    // SAFETY(E9): GetLocalTime 无参、返回 POD SYSTEMTIME 值，无指针契约。
    let local = unsafe { GetLocalTime() };
    let default_name = volttray_app::stats_export_filename_local(LocalTimeParts {
        year: local.wYear,
        month: local.wMonth,
        day: local.wDay,
        hour: local.wHour,
        minute: local.wMinute,
        second: local.wSecond,
        millis: local.wMilliseconds,
    });
    // U-2C（组5 5b）：保存前知情提示（Save-As 确认前）。文案随
    // 组4 N-08② 位图开关实配——位图默认关，未勾选时导出不含作息
    // 信息，提示不得虚报内容。
    if gates.privacy_notice {
        let body = if opts.export_activity_bitmap {
            "本文件含设备型号与作息信息（逐分钟活动位图）。\n\
             分享给他人前请确认接收方可信；勾选「匿名化设备键」可将设备标识替换为编号。"
        } else {
            "本文件含设备型号。\n\
             分享给他人前请确认接收方可信；勾选「匿名化设备键」可将设备标识替换为编号。"
        };
        notice("导出内容提示", body);
    }
    let Some(path) = crate::pick_save_path_with_filter(
        &default_name,
        "CSV 文件 (*.csv)\0*.csv\0所有文件 (*.*)\0*.*\0",
        "csv",
    ) else {
        crate::bootlog::note("stats export cancelled by user (save-as dialog)");
        return;
    };
    let _ = cmd_tx.send(IoCmd::ExportStats(StatsExportRequest {
        csv_path: path,
        html: opts.html,
        anonymize: opts.anonymize,
        json: opts.json,
        include_activity: opts.include_activity,
        redact_display_name: gates.redact_display_name,
        tz_utc: gates.tz_utc,
        export_activity_bitmap: opts.export_activity_bitmap,
    }));
}

// ---------------------------------------------------------------------------
// I-02：WM_ENDSESSION 承接窗口（常驻隐藏顶层窗口）
// ---------------------------------------------------------------------------

/// 关机承接钩子：GUI 线程在创建承接窗口前登记（cmd 通道 + flush 完成
/// 回执）。WM_ENDSESSION 广播到达时取出使用。
pub(crate) static ENDSESSION_HOOK: Mutex<Option<(mpsc::Sender<IoCmd>, mpsc::Receiver<()>)>> =
    Mutex::new(None);

// SAFETY(E9): 窗口过程回调：WM_ENDSESSION 路径经 ENDSESSION_HOOK 互斥锁取通道句柄，无锁外共享可变状态。
unsafe extern "system" fn endsession_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        // 允许关机（本程序不 veto）。
        WM_QUERYENDSESSION => LRESULT(1),
        WM_ENDSESSION => {
            // wParam != 0：关机真实发生 → 通知 IO 线程 flush（run_stop +
            // last_known + stats 双 store），最多等 2.5s 落盘完成——
            // 关机窗口期有限，超时即返回让系统继续（未落部分按崩溃路径
            // 「最后事件→run_start 离线段」口径归因，不静默）。
            if wparam.0 != 0 {
                if let Ok(guard) = ENDSESSION_HOOK.lock() {
                    if let Some((tx, rx)) = guard.as_ref() {
                        let _ = tx.send(IoCmd::Shutdown);
                        let _ = rx.recv_timeout(Duration::from_millis(2500));
                    }
                }
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// 创建常驻隐藏顶层窗口承接 WM_QUERYENDSESSION / WM_ENDSESSION（I-02）。
/// 父窗口为 None（桌面顶层）——绝不能用 HWND_MESSAGE：message-only
/// 窗口不接收广播消息。返回值忽略失败：承接窗口创建失败时关机 flush
/// 退化为 v3 崩溃路径口径（最后事件→run_start 离线段），不阻断启动。
pub fn install_endsession_window(cmd_tx: mpsc::Sender<IoCmd>, ack_rx: mpsc::Receiver<()>) {
    if let Ok(mut slot) = ENDSESSION_HOOK.lock() {
        *slot = Some((cmd_tx, ack_rx));
    }
    // SAFETY(E9): message-only 窗口创建按 Win32 契约；类名缓冲 NUL 结尾且存活至调用返回；失败路径留痕并安全退出。
    unsafe {
        let hinst = match windows::Win32::System::LibraryLoader::GetModuleHandleW(None) {
            Ok(h) => windows::Win32::Foundation::HINSTANCE(h.0),
            Err(e) => {
                crate::bootlog::note(&format!("endsession window: GetModuleHandleW failed: {e}"));
                return;
            }
        };
        let class = wide("VoltTrayEndSessionWnd");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(endsession_wndproc),
            hInstance: hinst,
            lpszClassName: PCWSTR(class.as_ptr()),
            ..Default::default()
        };
        let atom = RegisterClassW(&wc);
        if atom == 0 {
            let err = windows::Win32::Foundation::GetLastError();
            if err != windows::Win32::Foundation::ERROR_CLASS_ALREADY_EXISTS {
                crate::bootlog::note(&format!(
                    "endsession window: RegisterClassW failed: {err:?}"
                ));
                return;
            }
        }
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(class.as_ptr()),
            WS_OVERLAPPED, // 顶层窗口（不可见）；不挂 HWND_MESSAGE
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinst),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                crate::bootlog::note(&format!("endsession window: CreateWindowExW failed: {e}"));
                return;
            }
        };
        let _ = hwnd;
    }
}
