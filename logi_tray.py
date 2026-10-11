
"""
Logi VoltTray · 通用版托盘程序

外观、交互与旧版完全一致：
  * 双击程序  → 启动托盘图标
  * 左键图标  → 重新检测设备
  * 右键图标  → 菜单（重新检测设备 / 生成设备报告 / 开机启动 / 退出）
  * 图标上直接画电量数字，鼠标=蓝、键盘=绿，低电转橙/红

新增：
  * 显示真实设备名（如「MX Keys」），不再是笼统的"键盘 / 鼠标"；
  * 支持老款 HID++ 1.0 键鼠、蓝牙直连设备、有线设备；
  * 粗档位电量会显示成「约 90%」；
  * 菜单里能一键生成设备诊断报告，方便排查"读不到"的设备。
"""

import ctypes
import os
import sys
import threading
import time
from ctypes import wintypes

import logi_config as cfg
import logi_icon as ico
from logi_core import BatteryService, KIND_NAME


user32 = ctypes.WinDLL("user32", use_last_error=True)
gdi32 = ctypes.WinDLL("gdi32", use_last_error=True)
shell32 = ctypes.WinDLL("shell32", use_last_error=True)
kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)

LRESULT = ctypes.c_ssize_t
WNDPROC = ctypes.WINFUNCTYPE(LRESULT, wintypes.HWND, wintypes.UINT,
                             wintypes.WPARAM, wintypes.LPARAM)

WM_DESTROY = 2
WM_CLOSE = 16
WM_TIMER = 275
WM_COMMAND = 273
WM_RBUTTONUP = 517
WM_LBUTTONUP = 514
WM_CONTEXTMENU = 123

WM_APP = 32768
WM_TRAY = WM_APP + 1

NIM_ADD = 0
NIM_MODIFY = 1
NIM_DELETE = 2
NIF_MESSAGE = 1
NIF_ICON = 2
NIF_TIP = 4
NIF_INFO = 16
NIIF_NOSOUND = 16

SM_CXSMICON = 49
HWND_MESSAGE = -3

MF_STRING = 0
MF_SEPARATOR = 2048
MF_DEFAULT = 4096
MF_CHECKED = 8
TPM_RETURNCMD = 256
TPM_RIGHTBUTTON = 2

ID_RESCAN = 1002
ID_REPORT = 1004
ID_EXIT = 1003
ID_AUTOSTART = 1005

ERROR_ALREADY_EXISTS = 183

UNKNOWN_NAME = "无线设备"


class GUID(ctypes.Structure):
    _fields_ = [("Data1", wintypes.DWORD), ("Data2", wintypes.WORD),
                ("Data3", wintypes.WORD), ("Data4", ctypes.c_byte * 8)]


class NOTIFYICONDATAW(ctypes.Structure):
    _fields_ = [
        ("cbSize", wintypes.DWORD), ("hWnd", wintypes.HWND), ("uID", wintypes.UINT),
        ("uFlags", wintypes.UINT), ("uCallbackMessage", wintypes.UINT),
        ("hIcon", wintypes.HICON), ("szTip", ctypes.c_wchar * 128),
        ("dwState", wintypes.DWORD), ("dwStateMask", wintypes.DWORD),
        ("szInfo", ctypes.c_wchar * 256), ("uTimeoutOrVersion", wintypes.UINT),
        ("szInfoTitle", ctypes.c_wchar * 64), ("dwInfoFlags", wintypes.DWORD),
        ("guidItem", GUID), ("hBalloonIcon", wintypes.HICON)]


class WNDCLASSEXW(ctypes.Structure):
    _fields_ = [
        ("cbSize", wintypes.UINT), ("style", wintypes.UINT),
        ("lpfnWndProc", WNDPROC), ("cbClsExtra", ctypes.c_int),
        ("cbWndExtra", ctypes.c_int), ("hInstance", wintypes.HINSTANCE),
        ("hIcon", wintypes.HICON), ("hCursor", wintypes.HANDLE),
        ("hbrBackground", wintypes.HBRUSH), ("lpszMenuName", wintypes.LPCWSTR),
        ("lpszClassName", wintypes.LPCWSTR), ("hIconSm", wintypes.HICON)]


class MSG(ctypes.Structure):
    _fields_ = [("hwnd", wintypes.HWND), ("message", wintypes.UINT),
                ("wParam", wintypes.WPARAM), ("lParam", wintypes.LPARAM),
                ("time", wintypes.DWORD), ("pt", wintypes.POINT)]


user32.DefWindowProcW.restype = LRESULT
user32.DefWindowProcW.argtypes = [wintypes.HWND, wintypes.UINT,
                                  wintypes.WPARAM, wintypes.LPARAM]
user32.CreateWindowExW.restype = wintypes.HWND
user32.CreateWindowExW.argtypes = [wintypes.DWORD, wintypes.LPCWSTR, wintypes.LPCWSTR,
    wintypes.DWORD, ctypes.c_int, ctypes.c_int,
    ctypes.c_int, ctypes.c_int, wintypes.HWND,
    wintypes.HMENU, wintypes.HINSTANCE, ctypes.c_void_p]
user32.RegisterClassExW.restype = wintypes.ATOM
user32.RegisterClassExW.argtypes = [ctypes.POINTER(WNDCLASSEXW)]
user32.DestroyWindow.argtypes = [wintypes.HWND]
user32.GetMessageW.restype = ctypes.c_int
user32.GetMessageW.argtypes = [ctypes.POINTER(MSG), wintypes.HWND,
                               wintypes.UINT, wintypes.UINT]
user32.TranslateMessage.argtypes = [ctypes.POINTER(MSG)]
user32.DispatchMessageW.restype = LRESULT
user32.DispatchMessageW.argtypes = [ctypes.POINTER(MSG)]
user32.PostQuitMessage.argtypes = [ctypes.c_int]
user32.SetTimer.restype = ctypes.c_size_t
user32.SetTimer.argtypes = [wintypes.HWND, wintypes.UINT, wintypes.UINT, ctypes.c_void_p]
user32.KillTimer.argtypes = [wintypes.HWND, wintypes.UINT]
user32.GetSystemMetrics.restype = ctypes.c_int
user32.GetSystemMetrics.argtypes = [ctypes.c_int]
user32.DestroyIcon.argtypes = [wintypes.HICON]
user32.CreatePopupMenu.restype = wintypes.HMENU
user32.CreatePopupMenu.argtypes = []
user32.AppendMenuW.restype = wintypes.BOOL
user32.AppendMenuW.argtypes = [wintypes.HMENU, wintypes.UINT, wintypes.WPARAM,
                               wintypes.LPCWSTR]
user32.DestroyMenu.argtypes = [wintypes.HMENU]
user32.TrackPopupMenu.restype = wintypes.UINT
user32.TrackPopupMenu.argtypes = [wintypes.HMENU, wintypes.UINT, ctypes.c_int,
                                  ctypes.c_int, ctypes.c_int, wintypes.HWND,
                                  ctypes.c_void_p]
user32.GetCursorPos.argtypes = [ctypes.POINTER(wintypes.POINT)]
user32.SetForegroundWindow.argtypes = [wintypes.HWND]
user32.PostMessageW.restype = wintypes.BOOL
user32.PostMessageW.argtypes = [wintypes.HWND, wintypes.UINT,
                                wintypes.WPARAM, wintypes.LPARAM]
user32.SetProcessDpiAwarenessContext.restype = wintypes.BOOL
user32.SetProcessDpiAwarenessContext.argtypes = [ctypes.c_void_p]

shell32.Shell_NotifyIconW.restype = wintypes.BOOL
shell32.Shell_NotifyIconW.argtypes = [wintypes.DWORD, ctypes.POINTER(NOTIFYICONDATAW)]

kernel32.CreateMutexW.restype = wintypes.HANDLE
kernel32.CreateMutexW.argtypes = [ctypes.c_void_p, wintypes.BOOL, wintypes.LPCWSTR]
kernel32.CloseHandle.argtypes = [wintypes.HANDLE]
kernel32.GetModuleHandleW.restype = wintypes.HMODULE
kernel32.GetModuleHandleW.argtypes = [wintypes.LPCWSTR]


_REG_PATH = "Control Panel\\NotifyIconSettings"
_KEY_CACHE = "tray_keys.json"


def _claim_log(msg):
    cfg.log_file("claim.log", msg)


def _key_file():
    return os.path.join(cfg.data_dir(), _KEY_CACHE)


def _load_key_cache():
    try:
        import json
        with open(_key_file(), "r", encoding="utf-8") as f:
            v = json.load(f)
        return set(v) if isinstance(v, list) else set()
    except Exception:
        return set()


def _save_key_cache(keys):
    try:
        import json
        path = _key_file()
        tmp = path + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(sorted(keys), f)
        os.replace(tmp, path)
    except Exception:
        pass


def _icon_keys():
    try:
        import winreg
    except Exception:
        return set()
    out = set()
    try:
        k = winreg.OpenKey(winreg.HKEY_CURRENT_USER, _REG_PATH)
    except OSError:
        return out
    try:
        i = 0
        while True:
            try:
                out.add(winreg.EnumKey(k, i))
                i += 1
            except OSError:
                break
    finally:
        winreg.CloseKey(k)
    return out


def _key_values(sub):
    try:
        import winreg
        k = winreg.OpenKey(winreg.HKEY_CURRENT_USER, _REG_PATH)
        try:
            sk = winreg.OpenKey(k, sub)
        finally:
            winreg.CloseKey(k)
        try:
            d = {}
            j = 0
            while True:
                try:
                    n, v, t = winreg.EnumValue(sk, j)
                    j += 1
                    d[n] = v
                except OSError:
                    break
            return d
        finally:
            winreg.CloseKey(sk)
    except Exception:
        return {}


def _promote_keys(keys):
    try:
        import winreg
    except Exception:
        return 0
    try:
        k = winreg.OpenKey(winreg.HKEY_CURRENT_USER, _REG_PATH)
    except OSError:
        return 0
    n = 0
    try:
        for sub in keys:
            try:
                sk = winreg.OpenKey(k, sub, 0, winreg.KEY_READ | winreg.KEY_WRITE)
            except OSError:
                continue
            try:
                try:
                    cur = winreg.QueryValueEx(sk, "IsPromoted")[0]
                except OSError:
                    cur = None
                if cur != 1:
                    winreg.SetValueEx(sk, "IsPromoted", 0, winreg.REG_DWORD, 1)
                    n += 1
            finally:
                winreg.CloseKey(sk)
    finally:
        winreg.CloseKey(k)
    return n



class TrayIcon(object):
    def __init__(self, hwnd, uid):
        self.hwnd = hwnd
        self.uid = uid
        self.hicon = None
        self.added = False
        self.state = None
        self.rebuild = False

    def _nid(self, flags, tooltip):
        nid = NOTIFYICONDATAW()
        nid.cbSize = ctypes.sizeof(NOTIFYICONDATAW)
        nid.hWnd = self.hwnd
        nid.uID = self.uid
        nid.uFlags = flags
        nid.uCallbackMessage = WM_TRAY
        nid.hIcon = self.hicon or 0
        nid.szTip = (tooltip or "")[:127]
        return nid

    def _drop_shell_icon(self):
        if self.added:
            shell32.Shell_NotifyIconW(NIM_DELETE, ctypes.byref(self._nid(0, "")))
            self.added = False

    def balloon(self, text, title="Logi VoltTray"):
        if not self.added:
            return
        nid = self._nid(NIF_INFO, "")
        nid.szInfoTitle = title[:63]
        nid.szInfo = text[:255]
        nid.uTimeoutOrVersion = 3000
        nid.dwInfoFlags = NIIF_NOSOUND
        shell32.Shell_NotifyIconW(NIM_MODIFY, ctypes.byref(nid))

    def update(self, size, text, tooltip, pct, font, font_size, kind="", style="num"):
        state = (size, text, pct, font, font_size, kind, style)
        if state == self.state and self.added and not self.rebuild:
            return
        new_icon = ico.make_icon(size, text, pct, kind, font, font_size, style=style)
        if new_icon:
            if self.hicon:
                user32.DestroyIcon(self.hicon)
            self.hicon = new_icon
        if self.rebuild:
            self._drop_shell_icon()
            self.rebuild = False
        if not self.added:
            nid = self._nid(NIF_MESSAGE | NIF_ICON | NIF_TIP, tooltip)
            self.added = bool(shell32.Shell_NotifyIconW(NIM_ADD, ctypes.byref(nid)))
        else:
            nid = self._nid(NIF_ICON | NIF_TIP, tooltip)
            shell32.Shell_NotifyIconW(NIM_MODIFY, ctypes.byref(nid))
        self.state = state

    def remove(self):
        self._drop_shell_icon()
        if self.hicon:
            user32.DestroyIcon(self.hicon)
            self.hicon = None
        self.state = None
        self.rebuild = False



class TrayApp(object):
    CLASS = "LogiBatteryProTrayWnd"

    def __init__(self):
        self.settings = cfg.load_settings()
        self.svc = BatteryService(interval=2.0)
        self.hwnd = None
        self._proc = WNDPROC(self._wndproc)
        self.tips = {}
        self._uid_seq = 0
        self.size = max(16, user32.GetSystemMetrics(SM_CXSMICON))
        self._keys_before = _icon_keys()
        self._claimed = False
        self._claim_at = time.time() + 2.5
        self._claim_attempts = 0
        self._used_tips = set()
        self._reporting = False
        self._create_window()
        self.svc.start()

    def _next_uid(self):
        self._uid_seq += 1
        return self._uid_seq


    def _create_window(self):
        hInst = kernel32.GetModuleHandleW(None)
        wc = WNDCLASSEXW()
        wc.cbSize = ctypes.sizeof(WNDCLASSEXW)
        wc.lpfnWndProc = self._proc
        wc.hInstance = hInst
        wc.lpszClassName = self.CLASS
        if not user32.RegisterClassExW(ctypes.byref(wc)):
            err = ctypes.get_last_error()
            if err not in (0, 1410):
                raise ctypes.WinError(err)
        self.hwnd = user32.CreateWindowExW(
            0, self.CLASS, "Logi VoltTray", 0, 0, 0, 0, 0,
            wintypes.HWND(HWND_MESSAGE), None, hInst, None)
        if not self.hwnd:
            raise ctypes.WinError(ctypes.get_last_error())
        user32.SetTimer(self.hwnd, 1, 1000, None)


    def _wndproc(self, hwnd, msg, wparam, lparam):
        try:
            if msg == WM_TIMER:
                self._refresh()
                return 0
            if msg == WM_TRAY:
                event = lparam & 0xFFFF
                if event in (WM_RBUTTONUP, WM_CONTEXTMENU):
                    self._menu()
                elif event == WM_LBUTTONUP:
                    self.svc.request_rescan()
                    self._tips_show("正在重新检测罗技设备…")
                return 0
            if msg == WM_COMMAND:
                cid = wparam & 0xFFFF
                if cid == ID_RESCAN:
                    self.svc.request_rescan()
                    self._tips_show("正在重新检测罗技设备…")
                elif cid == ID_REPORT:
                    self._make_report()
                elif cid == ID_AUTOSTART:
                    self._toggle_autostart()
                elif cid == ID_EXIT:
                    user32.DestroyWindow(hwnd)
                return 0
            if msg == WM_CLOSE:
                user32.DestroyWindow(hwnd)
                return 0
            if msg == WM_DESTROY:
                for t in self.tips.values():
                    t.remove()
                self.tips.clear()
                self.svc.stop(wait=False)  # 退出路径不阻塞 UI 线程，守护线程随进程结束
                user32.KillTimer(hwnd, 1)
                user32.PostQuitMessage(0)
                return 0
        except Exception as exc:
            cfg.log_file("tray.log", "WndProc 异常 msg=0x%04X: %r" % (msg, exc))
        return user32.DefWindowProcW(hwnd, msg, wparam, lparam)

    def _menu(self):
        pt = wintypes.POINT()
        user32.GetCursorPos(ctypes.byref(pt))
        hmenu = user32.CreatePopupMenu()
        user32.AppendMenuW(hmenu, MF_STRING | MF_DEFAULT, ID_RESCAN, "重新检测设备")
        user32.AppendMenuW(hmenu, MF_STRING, ID_REPORT, "生成设备报告（排查用）")
        user32.AppendMenuW(hmenu, MF_SEPARATOR, 0, None)
        # 勾选状态以注册表为准，每次弹菜单时现读
        flags = MF_STRING | (MF_CHECKED if cfg.autostart_enabled() else 0)
        user32.AppendMenuW(hmenu, flags, ID_AUTOSTART, "开机启动")
        user32.AppendMenuW(hmenu, MF_STRING, ID_EXIT, "退出")
        user32.SetForegroundWindow(self.hwnd)
        cmd = user32.TrackPopupMenu(hmenu, TPM_RETURNCMD | TPM_RIGHTBUTTON,
                                    pt.x, pt.y, 0, self.hwnd, None)
        user32.DestroyMenu(hmenu)
        if cmd:
            user32.PostMessageW(self.hwnd, WM_COMMAND, cmd, 0)

    def _tips_show(self, text):
        for t in self.tips.values():
            t.balloon(text)
            break

    def _toggle_autostart(self):
        """「开机启动」开关：切换注册表 Run 键（注册表为唯一事实源），
        settings.json 只做镜像；成功/失败都弹气泡告知用户"""
        try:
            new, ok = cfg.toggle_autostart()
        except Exception as exc:
            cfg.log_file("tray.log", f"切换开机启动异常: {exc!r}")
            new, ok = cfg.autostart_enabled(), False
        if ok:
            cfg.log_file("tray.log", "开机启动已%s" % ("开启" if new else "关闭"))
            self.settings["autostart"] = new
            self._tips_show("已%s开机启动" % ("开启" if new else "关闭"))
        else:
            cfg.log_file("tray.log", "切换开机启动失败")
            self._tips_show("开机启动设置失败，详见 tray.log")

    def _make_report(self):
        """生成设备诊断报告（放后台线程，避免界面卡住）"""
        if self._reporting:
            return
        self._reporting = True
        threading.Thread(target=self._report_worker, daemon=True).start()

    def _report_worker(self):
        try:
            import logi_report

            # 停止并等服务线程真正退出（释放 HID 通道）再采集，避免两边抢设备句柄
            self.svc.stop(wait=True, timeout=10.0)
            time.sleep(0.6)
            try:
                text = logi_report.collect()
            finally:
                self.svc = BatteryService(interval=2.0)
                self.svc.start()
            path = os.path.join(cfg.data_dir(), "device_report.txt")
            with open(path, "w", encoding="utf-8") as f:
                f.write(text)
            self._tips_show("报告已生成，正在打开目录")
            try:
                os.startfile(cfg.data_dir())
            except Exception:
                pass
        except Exception as exc:
            cfg.log_file("tray.log", f"生成报告失败: {exc!r}")
            self._tips_show("生成报告失败，详见 tray.log")
        finally:
            self._reporting = False


    def _refresh(self):
        snap = self.svc.snapshot()

        keys = []
        font = self.settings.get("font") or ""
        try:
            fsize = int(self.settings.get("font_size") or 0)
        except (TypeError, ValueError):
            fsize = 0
        style = self.settings.get("style") or cfg.DEFAULT_STYLE

        for dev in snap:
            k = dev.get("id") or dev.get("name") or "?"
            nm = dev["name"] or UNKNOWN_NAME
            keys.append(k)
            if k not in self.tips:
                self.tips[k] = TrayIcon(self.hwnd, self._next_uid())
            pct = dev["percent"]
            kind_cn = KIND_NAME.get(dev["kind"], "")
            if pct is None:
                text = "--"
                tip = f"{nm} {kind_cn} 电量读取中"
            else:
                text = str(pct)
                tip = "%s %s %d%%" % (nm, kind_cn, pct)
                if dev.get("approx"):
                    tip = "%s %s 约%d%%" % (nm, kind_cn, pct)
                if dev.get("charging"):
                    tip += "（充电中）"
                elif dev.get("plugged"):
                    tip += "（已接电源）"
            if len(self._used_tips) < 128:
                self._used_tips.add(tip)
            self.tips[k].update(self.size, text, tip, pct, font, fsize,
                                dev.get("kind") or "", style)

        for k in list(self.tips):
            if k not in keys:
                self.tips[k].remove()
                del self.tips[k]

        if not snap:
            nm = "status"
            if nm not in self.tips:
                self.tips[nm] = TrayIcon(self.hwnd, self._next_uid())
            tip = "正在检测罗技设备…" if not self.svc.ready else "未找到罗技设备"
            if len(self._used_tips) < 128:
                self._used_tips.add(tip)
            self.tips[nm].update(self.size, "--", tip, None, font, fsize, "", style)

        if not self._claimed and time.time() >= self._claim_at:
            self._claim_at = time.time() + 2.0
            try:
                self._claim_attempts += 1
                if self._claim_attempts > 90:
                    # 认领失败不能无限重试：每 2 秒一次全量注册表枚举 + 日志，
                    # 长期运行会把 claim.log 写爆。放弃不影响图标显示本身。
                    _claim_log("认领重试超过 90 次仍未成功，停止重试（不影响显示）")
                    self._claimed = True
                elif self._claim_tray():
                    self._claimed = True
            except Exception:
                pass

    def _claim_tray(self):
        """把自己的托盘图标登记为「固定在任务栏显示」"""
        try:
            import winreg
        except Exception as exc:
            _claim_log(f"winreg 不可用: {exc!r}")
            return True
        now = _icon_keys()
        fresh = now - self._keys_before
        keys = set(_load_key_cache())
        _claim_log("before=%d now=%d fresh=%d cache=%d"
                   % (len(self._keys_before), len(now), len(fresh), len(keys)))
        keys |= fresh
        if not keys:
            # 回退匹配只认本程序实际用过的 tooltip 和带「罗技」的专属前缀，
            # 不用「鼠标/键盘/MX」这类宽泛前缀——那会误改其他程序的托盘设置
            for sub in now:
                tip = str(_key_values(sub).get("InitialTooltip", ""))
                if tip in self._used_tips or any(tip.startswith(p) for p in
                       ('罗技', '未找到', '正在检测')):
                    keys.add(sub)
            _claim_log("回退匹配得到 %d 个" % len(keys))
        if not keys:
            _claim_log("没有可认领的条目，稍后重试")
            return False
        n = _promote_keys(keys)
        _save_key_cache(keys)
        _claim_log("已固定 %d 个条目（本次修改 %d）" % (len(keys), n))
        return True


    def run(self):
        msg = MSG()
        while user32.GetMessageW(ctypes.byref(msg), None, 0, 0) > 0:
            user32.TranslateMessage(ctypes.byref(msg))
            user32.DispatchMessageW(ctypes.byref(msg))
        return 0


def _single_instance():
    try:
        h = kernel32.CreateMutexW(None, True, "Local\\LogiBatteryProTray_SingleInstance")
        if not h:
            return None
        if ctypes.get_last_error() == ERROR_ALREADY_EXISTS:
            kernel32.CloseHandle(h)
            return None
        return h
    except Exception:
        return None


def _heal_autostart():
    """启动时对齐开机启动状态——注册表是唯一事实源。

    * settings 只做镜像：把注册表实际状态写回 settings（绝不按 settings
      反向"复活"注册表项——用户外部删除的项必须保持删除）；
    * 自愈增强（可选）：注册表值仍存在、但指向已移动的旧路径时，
      按当前程序路径重写。注意这与"复活"不同：值本来就存在，只是修正路径。
    """
    try:
        s = cfg.load_settings()
        enabled = cfg.autostart_enabled()
        if not enabled and cfg.autostart_value() is not None:
            # 值还在但路径已失效（程序被移动/改名）：重写为当前正确路径
            cfg.set_autostart(True)
            enabled = cfg.autostart_enabled()
        if bool(s.get("autostart")) != enabled:
            s["autostart"] = enabled
            cfg.save_settings(s)
    except Exception as exc:
        cfg.log_file("tray.log", f"对齐开机启动状态失败: {exc!r}")


def main():
    try:
        user32.SetProcessDpiAwarenessContext(ctypes.c_void_p(-4))
    except Exception:
        pass

    lock = _single_instance()
    if lock is None:
        return 0

    # 开机启动以注册表为唯一事实源：外部删除不复活，仅镜像回 settings + 陈旧路径自愈
    _heal_autostart()
    try:
        TrayApp().run()
        return 0
    except Exception:
        import traceback
        cfg.log_file("tray.log", "启动失败:\n" + traceback.format_exc())
        return 1


if __name__ == "__main__":
    sys.exit(main())
