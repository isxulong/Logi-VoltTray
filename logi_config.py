# -*- coding: utf-8 -*-
"""
Logi VoltTray · 通用版配置

外观参数（字体、字号、样式、开机启动）与旧版完全一致，数据目录换成
LogiBatteryPro，避免和旧版互相覆盖。
"""

import ctypes
import json
import os
import sys
import tempfile
import time
from ctypes import wintypes

user32 = ctypes.WinDLL('user32', use_last_error=True)
gdi32 = ctypes.WinDLL('gdi32', use_last_error=True)
kernel32 = ctypes.WinDLL('kernel32', use_last_error=True)

APP_NAME = 'LogiBatteryPro'
RUN_KEY = r'Software\Microsoft\Windows\CurrentVersion\Run'
THEME_KEY = r'Software\Microsoft\Windows\CurrentVersion\Themes\Personalize'

DEFAULT_FONT = 'Segoe UI'
DEFAULT_FONT_SIZE = 0
DEFAULT_STYLE = 'num'

DEFAULTS = {
    'font': DEFAULT_FONT,
    'font_size': DEFAULT_FONT_SIZE,
    'autostart': False,
    'style': DEFAULT_STYLE}



COMMON_FONTS = ['Segoe UI', '微软雅黑', 'Microsoft YaHei UI', 'Arial', 'Times New Roman', 'Tahoma', 'Verdana', 'Georgia', 'Calibri', 'Cambria', 'Trebuchet MS', 'Consolas', 'Courier New', 'Arial Narrow', '宋体', '黑体', '等线']







def data_dir():
    base = (os.environ.get('LOCALAPPDATA') or os.environ.get('TEMP')
            or tempfile.gettempdir() or '.')
    d = os.path.join(base, APP_NAME)
    try:
        os.makedirs(d, exist_ok=True)
    except Exception:
        pass
    return d


def settings_file():
    return os.path.join(data_dir(), 'settings.json')


_LOG_MAX_BYTES = 512 * 1024
_LOG_KEEP_BYTES = 128 * 1024
# 公共别名（供测试与外部断言引用）
LOG_MAX_BYTES = _LOG_MAX_BYTES
LOG_KEEP_BYTES = _LOG_KEEP_BYTES


def log_file(name, msg):
    try:
        path = os.path.join(data_dir(), name)
        try:
            if os.path.getsize(path) > _LOG_MAX_BYTES:
                # 日志限长：超过 512KB 只保留末尾 128KB，防止长期运行把磁盘写满
                with open(path, 'rb') as f:
                    f.seek(-_LOG_KEEP_BYTES, os.SEEK_END)
                    tail = f.read()
                with open(path, 'wb') as f:
                    f.write(b'... (older log truncated) ...\n' + tail)
        except OSError:
            pass
        with open(path, 'a', encoding='utf-8') as f:
            f.write(f"{time.strftime('%Y-%m-%d %H:%M:%S')!s} {msg!s}\n")
    except Exception:
        pass



def load_settings():
    s = dict(DEFAULTS)
    first_run = not os.path.exists(settings_file())
    try:
        with open(settings_file(), 'r', encoding='utf-8') as f:
            data = json.load(f)
        if isinstance(data, dict):
            for k in DEFAULTS:
                if k in data:
                    s[k] = data[k]
    except Exception:
        pass
    if first_run:
        # 首次运行：尝试从旧版（LogiBattery）目录迁移外观设置，
        # 只搬 font / font_size / style 三项。

        try:
            base = os.environ.get('LOCALAPPDATA') or os.environ.get('TEMP') or '.'
            with open(os.path.join(base, 'LogiBattery', 'settings.json'), 'r',
                      encoding='utf-8') as f:
                old = json.load(f)
            if isinstance(old, dict):
                for k in ('font', 'font_size', 'style'):
                    if old.get(k) not in (None, ''):
                        s[k] = old[k]
                save_settings(s)
        except Exception:
            pass
    return s


def save_settings(s):
    try:
        path = settings_file()
        tmp = path + '.tmp'
        with open(tmp, 'w', encoding='utf-8') as f:
            json.dump(s, f, ensure_ascii=False, indent=2)
        os.replace(tmp, path)
        return True
    except Exception:
        return False



class LOGFONTW(ctypes.Structure):
    _fields_ = [
        ('lfHeight', ctypes.c_long), ('lfWidth', ctypes.c_long),
        ('lfEscapement', ctypes.c_long), ('lfOrientation', ctypes.c_long),
        ('lfWeight', ctypes.c_long), ('lfItalic', ctypes.c_byte),
        ('lfUnderline', ctypes.c_byte), ('lfStrikeOut', ctypes.c_byte),
        ('lfCharSet', ctypes.c_byte), ('lfOutPrecision', ctypes.c_byte),
        ('lfClipPrecision', ctypes.c_byte), ('lfQuality', ctypes.c_byte),
        ('lfPitchAndFamily', ctypes.c_byte), ('lfFaceName', ctypes.c_wchar * 32)]



FONTENUMPROC = ctypes.WINFUNCTYPE(
    ctypes.c_int, ctypes.POINTER(LOGFONTW), ctypes.c_void_p,
    wintypes.DWORD, wintypes.LPARAM)

gdi32.EnumFontFamiliesExW.restype = ctypes.c_int
gdi32.EnumFontFamiliesExW.argtypes = [wintypes.HDC, ctypes.POINTER(LOGFONTW),
                                      FONTENUMPROC, wintypes.LPARAM, wintypes.DWORD]
user32.GetDC.restype = wintypes.HDC
user32.GetDC.argtypes = [wintypes.HWND]
user32.ReleaseDC.argtypes = [wintypes.HWND, wintypes.HDC]

_fonts_cache = None
_theme_cache = [0.0, False]


def installed_fonts():
    '系统已安装的字体名集合'
    global _fonts_cache
    if _fonts_cache is not None:
        return _fonts_cache
    names = set()
    try:
        lf = LOGFONTW()
        lf.lfCharSet = 1
        hdc = user32.GetDC(None)

        def _cb(lpelfe, lpntme, fonttype, lparam):
            try:
                face = lpelfe.contents.lfFaceName
                if face and not face.startswith('@'):
                    names.add(face)
            except Exception:
                pass
            return 1

        proc = FONTENUMPROC(_cb)
        gdi32.EnumFontFamiliesExW(hdc, ctypes.byref(lf), proc, 0, 0)
        user32.ReleaseDC(None, hdc)
    except Exception:
        pass
    _fonts_cache = names
    return names


def font_choices():
    '下拉框里要展示的字体列表'
    have = installed_fonts()
    if not have:
        return list(COMMON_FONTS)
    out = [f for f in COMMON_FONTS if f in have]
    if DEFAULT_FONT not in out:
        out.insert(0, DEFAULT_FONT)
    return out


def theme_is_light():
    '系统任务栏是浅色主题吗（10 秒缓存）'
    now = time.time()
    if now - _theme_cache[0] < 10.0:
        return _theme_cache[1]
    light = False
    try:
        import winreg
        k = winreg.OpenKey(winreg.HKEY_CURRENT_USER, THEME_KEY)
        try:
            light = bool(winreg.QueryValueEx(k, 'SystemUsesLightTheme')[0])
        finally:
            winreg.CloseKey(k)
    except Exception:
        pass
    _theme_cache[0] = now
    _theme_cache[1] = light
    return light



def launcher_path():
    """用户实际双击的那个 exe。

    PyInstaller 单文件模式会把程序解压到临时目录再运行，
    此时 sys.executable 指向临时副本，因此需要回溯父进程取原始路径。
    """
    exe = sys.executable or ''
    if '_MEI' not in exe:
        return exe
    try:
        k32 = kernel32

        class PROCESSENTRY32W(ctypes.Structure):
            _fields_ = [
                ('dwSize', wintypes.DWORD), ('cntUsage', wintypes.DWORD),
                ('th32ProcessID', wintypes.DWORD), ('th32DefaultHeapID', ctypes.c_void_p),
                ('th32ModuleID', wintypes.DWORD), ('cntThreads', wintypes.DWORD),
                ('th32ParentProcessID', wintypes.DWORD),
                ('pcPriClassBase', ctypes.c_long), ('dwFlags', wintypes.DWORD),
                ('szExeFile', ctypes.c_wchar * 260)]

        k32.CreateToolhelp32Snapshot.restype = wintypes.HANDLE
        k32.CreateToolhelp32Snapshot.argtypes = [wintypes.DWORD, wintypes.DWORD]
        k32.Process32FirstW.argtypes = [wintypes.HANDLE, ctypes.POINTER(PROCESSENTRY32W)]
        k32.Process32NextW.argtypes = [wintypes.HANDLE, ctypes.POINTER(PROCESSENTRY32W)]
        k32.OpenProcess.restype = wintypes.HANDLE
        k32.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
        k32.QueryFullProcessImageNameW.restype = wintypes.BOOL
        k32.QueryFullProcessImageNameW.argtypes = [wintypes.HANDLE, wintypes.DWORD,
                                                   wintypes.LPWSTR,
                                                   ctypes.POINTER(wintypes.DWORD)]
        k32.CloseHandle.argtypes = [wintypes.HANDLE]

        me = k32.GetCurrentProcessId()
        parent = 0
        snap = k32.CreateToolhelp32Snapshot(2, 0)
        pe = PROCESSENTRY32W()
        pe.dwSize = ctypes.sizeof(PROCESSENTRY32W)
        if k32.Process32FirstW(snap, ctypes.byref(pe)):
            while True:
                if pe.th32ProcessID == me:
                    parent = pe.th32ParentProcessID
                    break
                if not k32.Process32NextW(snap, ctypes.byref(pe)):
                    break
        k32.CloseHandle(snap)
        if parent:
            h = k32.OpenProcess(4096, False, parent)
            if h:
                try:
                    buf = ctypes.create_unicode_buffer(1024)
                    size = wintypes.DWORD(1024)
                    ok = k32.QueryFullProcessImageNameW(h, 0, buf, ctypes.byref(size))
                finally:
                    k32.CloseHandle(h)
                if ok and buf.value.lower().endswith('.exe'):
                    return buf.value
    except Exception:
        pass
    return exe


def _read_autostart_value():
    """读注册表 Run 键里本程序的值（原始字符串），没有返回 None。"""
    try:
        import winreg
    except Exception:
        return None
    try:
        k = winreg.OpenKey(winreg.HKEY_CURRENT_USER, RUN_KEY)
    except OSError:
        return None
    try:
        return winreg.QueryValueEx(k, APP_NAME)[0]
    except OSError:
        return None
    finally:
        try:
            winreg.CloseKey(k)
        except Exception:
            pass


def _exe_from_run_value(value):
    """从 Run 键值里提取 exe 路径（去掉首尾引号与参数）。"""
    if not value:
        return ''
    v = str(value).strip()
    if v.startswith('"'):
        end = v.find('"', 1)
        return v[1:end] if end > 1 else v.strip('"')
    return v.split(' ', 1)[0]


def _same_exe(a, b):
    if not a or not b:
        return False
    try:
        return os.path.normcase(os.path.normpath(a)) == os.path.normcase(os.path.normpath(b))
    except Exception:
        return False


def autostart_enabled():
    """开机启动是否已开启——以注册表为准。

    值存在且指向当前程序路径才算开启；指向别处（程序已移动 / 旧版残留）
    视为未开启，下次开启时会改写成正确路径。
    """
    value = _read_autostart_value()
    if value is None:
        return False
    return _same_exe(_exe_from_run_value(value), launcher_path())


def autostart_value():
    """当前注册表 Run 键里保存的自启动命令行；没有则返回 None"""
    return _read_autostart_value()


def expected_run_value():
    """应当写入 Run 键的命令行（带引号的程序自身路径 + --startup）。

    PyInstaller onefile 模式下若无法回溯到原始 exe（launcher_path 落到
    会随退出而删除的 _MEI 临时副本），返回空串——这种路径写进注册表
    重启后必然失效，调用方必须拒绝写入。
    """
    exe = launcher_path()
    if not exe or '_MEI' in exe:
        return ''
    return '"%s" --startup' % exe


def set_autostart(enable):
    """写入 / 删除注册表 Run 键（HKCU，免管理员权限）。返回是否成功。"""
    try:
        import winreg
    except Exception as exc:
        log_file('config.log', f'set_autostart({enable!s}) 失败: winreg 不可用 {exc!r}')
        return False
    k = None
    try:
        # CreateKey：Run 键不存在时也能工作（OpenKey 会直接失败）
        k = winreg.CreateKey(winreg.HKEY_CURRENT_USER, RUN_KEY)
        if enable:
            value = expected_run_value()
            if not value:
                # onefile 回溯失败（_MEI 临时路径）：写进去重启后必然失效，拒绝写入
                log_file('config.log',
                         'set_autostart: 无法确定有效的启动器路径（onefile 临时目录），放弃写入')
                return False
            winreg.SetValueEx(k, APP_NAME, 0, winreg.REG_SZ, value)
            # 写后读回校验，避免静默失败
            return _same_exe(_exe_from_run_value(_read_autostart_value()),
                             launcher_path())
        try:
            winreg.DeleteValue(k, APP_NAME)
        except OSError:
            pass  # 值本就不存在，删除视为成功
        return _read_autostart_value() is None
    except Exception as exc:
        log_file('config.log', f'set_autostart({enable!s}) 失败: {exc!r}')
        return False
    finally:
        if k is not None:
            try:
                winreg.CloseKey(k)
            except Exception:
                pass


def toggle_autostart():
    """切换开机启动。返回 (新状态, 是否成功)；注册表为准，settings 只做镜像。"""
    cur = autostart_enabled()
    if not set_autostart(not cur):
        return cur, False
    new = not cur
    try:
        s = load_settings()
        s['autostart'] = new
        save_settings(s)
    except Exception:
        pass  # 镜像写失败不影响注册表事实
    return new, True


if __name__ == '__main__':
    print('配置目录:', data_dir())
    print('当前设置:', load_settings())
    print('启动器路径:', launcher_path())
    print('开机自启:', autostart_enabled())
    print('系统浅色主题:', theme_is_light())
    print('可选字体:', font_choices())
