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
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
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
}

/// 导出选项（对话框三开关；默认：HTML 开、匿名化关、JSON 关——
/// physical_key 默认保留（本机自用口径，v3 文档标注敏感级），
/// HTML 报告为本期拍板特性默认随导出生成）。
#[derive(Debug, Clone, Copy)]
pub struct ExportOptions {
    pub html: bool,
    pub anonymize: bool,
    pub json: bool,
}

const IDC_HTML: u32 = 1001;
const IDC_ANON: u32 = 1002;
const IDC_JSON: u32 = 1003;
const ID_EXPORT_OK: u32 = 1101;
const ID_EXPORT_CANCEL: u32 = 1102;

struct DialogState {
    result: Option<ExportOptions>,
}

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
                if !state_ptr.is_null() {
                    if id == ID_EXPORT_OK {
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
                        });
                    }
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

/// 导出选项对话框（模态）：三个勾选 + 确定/取消。取消返回 None
/// （零落盘、零派发）。进度提示（v3 P2-③）：对话框内静态文案告知
/// 「后台执行、完成弹窗」，渲染期间 UI 不被阻塞（派往 IO 线程）。
pub fn pick_export_options() -> Option<ExportOptions> {
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
        let x = (GetSystemMetrics(SM_CXSCREEN) - 360) / 2;
        let y = (GetSystemMetrics(SM_CYSCREEN) - 240) / 2;
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            x.max(0),
            y.max(0),
            360,
            240,
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
        // 控件：说明 + 三勾选 + 确定/取消（子窗口随父窗口销毁，无需显式释放）。
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
            CreateWindowExW(
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
            )
        };
        let _ = mk(
            "STATIC",
            "选择导出内容。导出在后台执行，完成后弹窗提示结果。",
            WINDOW_STYLE(0),
            12,
            12,
            330,
            32,
            0,
        );
        let h_html = mk(
            "BUTTON",
            "同时生成可视化报告（HTML，同名主干 .html）",
            WS_GROUP | WS_TABSTOP | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
            16,
            52,
            320,
            20,
            IDC_HTML,
        )
        .unwrap_or_default();
        let _ = mk(
            "BUTTON",
            "匿名化设备键（device_key 显示为 D1、D2…）",
            WS_GROUP | WS_TABSTOP | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
            16,
            78,
            320,
            20,
            IDC_ANON,
        );
        let _ = mk(
            "BUTTON",
            "同时导出原始事件序列（JSON，同名主干 .json）",
            WS_GROUP | WS_TABSTOP | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
            16,
            104,
            320,
            20,
            IDC_JSON,
        );
        let _ = mk(
            "BUTTON",
            "选择保存位置…",
            WS_GROUP | WS_TABSTOP | WINDOW_STYLE(BS_DEFPUSHBUTTON as u32),
            60,
            150,
            110,
            28,
            ID_EXPORT_OK,
        );
        let _ = mk(
            "BUTTON",
            "取消",
            WS_GROUP | WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32),
            190,
            150,
            110,
            28,
            ID_EXPORT_CANCEL,
        );
        // 默认勾选 HTML 报告（本期拍板特性）。
        SendMessageW(
            h_html,
            BM_SETCHECK,
            Some(WPARAM(BST_CHECKED.0 as usize)),
            None,
        );
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = windows::Win32::Graphics::Gdi::UpdateWindow(hwnd);
        // 模态循环：对话框销毁时 PostQuitMessage 退出本循环。
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        state.result
    }
}

/// 「导出耗电统计…」菜单编排：选项对话框 → 秒级默认命名（R5-P-02）→
/// Save-As（CSV 过滤器）→ 派发 IO 线程。任一步取消即零落盘零派发。
pub fn begin_export(cmd_tx: &mpsc::Sender<IoCmd>) {
    let Some(opts) = pick_export_options() else {
        crate::bootlog::note("stats export cancelled at options dialog");
        return;
    };
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
    }));
}

// ---------------------------------------------------------------------------
// I-02：WM_ENDSESSION 承接窗口（常驻隐藏顶层窗口）
// ---------------------------------------------------------------------------

/// 关机承接钩子：GUI 线程在创建承接窗口前登记（cmd 通道 + flush 完成
/// 回执）。WM_ENDSESSION 广播到达时取出使用。
pub(crate) static ENDSESSION_HOOK: Mutex<Option<(mpsc::Sender<IoCmd>, mpsc::Receiver<()>)>> =
    Mutex::new(None);

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
