# -*- coding: utf-8 -*-
"""Windows 环境桩（测试专用，非产品代码）。

让依赖 ctypes.WinDLL / ctypes.WINFUNCTYPE / winreg / hid 的产品模块
能在 Linux 下被导入，并对纯逻辑（注册表读写、菜单构造、应答解析等）
做真实可执行的单元测试。

用法：每个测试模块在导入任何 logi_* 模块之前，先 `import _winstub`。幂等。

提供：
  * ctypes.WINFUNCTYPE -> CFUNCTYPE（Linux 没有 WINAPI 调用约定）
  * ctypes.WinDLL      -> FakeDLL 工厂，所有 API 调用记录到 CALLS，
                          可用 HANDLERS / DEFAULT_RETURNS 定制返回值
  * sys.modules['winreg'] -> 内存版注册表（HKCU 子树，路径即句柄）
  * sys.modules['hid']    -> 空 HID 后端（enumerate 返回空）

桩行为对照（E20，2026-10-24 红蓝对抗终审补注）
=============================================
每条桩与真实 Windows 行为的对应关系，以及「不像真的」的已知边界。
改桩或被桩绊住时先读这张表：对不上的差异就是「测试在验证一个不存在
的世界」——E20/A-R8 关注的正是这类缺口代价（uhid 真机 runner 上线前，
本套件只覆盖纯逻辑层，内核设备层行为一律不在断言范围内）。

ctypes.WINFUNCTYPE -> CFUNCTYPE
  真实：WINFUNCTYPE 使用 WINAPI stdcall 调用约定（仅 Windows 存在）。
  桩：Linux 无 stdcall，退化为 CFUNCTYPE（cdecl）。
  边界：只影响调用约定，不影响参数个数与返回值语义；回调被真实
  Windows 以 stdcall 调用的 ABI 差异本桩不覆盖。

ctypes.WinDLL -> FakeDLL（CALLS / HANDLERS / DEFAULT_RETURNS）
  真实：LoadLibrary 加载系统 DLL，API 返回句柄 / 错误码 / 设
  last-error；句柄值由系统分配、不可预知。
  桩：DEFAULT_RETURNS 里的句柄值（100/55/0x500/0xABC/77 等）是
  随便取的「非零魔法数」，只为让产品代码走「成功」分支；未登记的
  API 一律返回 0——对「返回非 0 即成功」的 API 是失败分支，对
  「返回 0 即失败」的 BOOL 类 API 恰好也是失败分支，方向一致。
  边界：不模拟 last-error、不模拟句柄生命周期（真实句柄会失效，
  见 E2）；需要失败路径时用 HANDLERS 显式覆写，别依赖魔法数方向。

FakeWinreg（内存版 HKCU）
  真实：winreg 句柄是系统对象；OpenKey 不存在抛
  FileNotFoundError(OSError(2))；枚举越界抛 OSError(22)；
  Run 键在正常系统上总是存在。
  桩：句柄 = 规范化小写路径字符串；OpenKey/QueryValueEx/DeleteValue
  对不存在的键抛同样的 OSError(2)，EnumKey/EnumValue 越界抛
  OSError(22)——errno 与真实一致，断言 errno 的用例有效。
  边界：① 只覆盖 HKCU，其它根键不存在；② 不模拟权限错误
  （OSError(5)）；③ 不模拟其它进程并发改注册表；④ 大小写归一
  与真实 Windows 一致（键名不区分大小写）。

hid 后端（空枚举）
  真实：hid.enumerate 列出本机 HID 设备；device.open_path 打开
  设备节点读写报告。
  桩：enumerate 恒返回空列表（等同「没插任何设备」）；open_path
  直接抛 OSError。
  边界：设备插拔、报告字节流、句柄失效（E2）等一切设备层行为
  不在本桩覆盖范围——那正是 uhid 真机 runner（E20 排期项）要补的层。
"""
import ctypes
import sys
import types

# 桩在所有平台强制生效（含 win32）：协议冻结门禁在 windows-latest CI 上
# 运行本测试套件，真实 winreg / Win32 DLL 会让用例脱离受控环境
# （真实注册表无 remove_path、真实父进程链返回 runner 的 pwsh.exe 等）。
# 用例全部按桩环境编写（Linux 80/80 验证），桩必须全平台一致。
ctypes.WINFUNCTYPE = ctypes.CFUNCTYPE

CALLS = []          # [(dll_name, func_name, args), ...] 全部 Win32 调用记录
HANDLERS = {}       # {func_name 或 (dll_name, func_name): callable} 自定义行为
DEFAULT_RETURNS = {
    'GetDC': 1,
    'CreatePopupMenu': 100,
    'TrackPopupMenu': 0,
    'CreateMutexW': 55,
    'GetCurrentProcessId': 4242,
    'CreateToolhelp32Snapshot': 0x500,
    'OpenProcess': 0xABC,
    'SetTimer': 1,
    'GetMessageW': 0,
    'RegisterClassW': 1,
    'CreateWindowExW': 77,
}


class FakeFunc(object):
    """假冒的 Win32 API：记录调用、可分派到 HANDLERS，允许赋 restype/argtypes"""

    def __init__(self, dll, name):
        self.dll = dll
        self.name = name
        self.restype = None
        self.argtypes = None

    def __call__(self, *args):
        CALLS.append((self.dll, self.name, args))
        h = HANDLERS.get((self.dll, self.name))
        if h is None:
            h = HANDLERS.get(self.name)
        if h is not None:
            return h(*args)
        return DEFAULT_RETURNS.get(self.name, 0)


class FakeDLL(object):
    def __init__(self, name):
        object.__setattr__(self, '_dll_name', name)
        object.__setattr__(self, '_funcs', {})

    def __getattr__(self, name):
        funcs = object.__getattribute__(self, '_funcs')
        f = funcs.get(name)
        if f is None:
            f = FakeFunc(object.__getattribute__(self, '_dll_name'), name)
            funcs[name] = f
        return f


DLLS = {}


def _fake_windll(name, use_last_error=False):
    d = DLLS.get(name)
    if d is None:
        d = DLLS[name] = FakeDLL(name)
    return d


ctypes.WinDLL = _fake_windll


class FakeWinreg(types.ModuleType):
    """内存版 HKCU 注册表。句柄 = 规范化后的小写全路径字符串。"""

    HKEY_CURRENT_USER = 'HKCU'
    KEY_SET_VALUE = 0x0002
    KEY_READ = 0x20019
    KEY_WRITE = 0x20006
    REG_SZ = 1
    REG_DWORD = 4

    def __init__(self):
        super(FakeWinreg, self).__init__('winreg')
        self.reset()

    def reset(self):
        self._tree = {'subkeys': {}, 'values': {}}
        self.calls = []          # [(api, path), ...] 调用记录（断言 CreateKey/CloseKey 配对用）
        # Windows 上 Run 键总是存在
        self.create_path(r'Software\Microsoft\Windows\CurrentVersion\Run')

    # -- 内部 ----------------------------------------------------------
    def _norm(self, parent, sub):
        parts = []
        if parent not in (None, '', self.HKEY_CURRENT_USER):
            parts.append(str(parent))
        if sub:
            parts.append(str(sub))
        return '\\'.join(parts).strip('\\').lower()

    def create_path(self, path):
        node = self._tree
        for part in path.split('\\'):
            node = node['subkeys'].setdefault(part.lower(),
                                              {'subkeys': {}, 'values': {}})
        return node

    def _node(self, path):
        node = self._tree
        if not path:
            return node
        for part in path.split('\\'):
            node = node['subkeys'].get(part.lower())
            if node is None:
                raise OSError(2, 'The system cannot find the file specified')
        return node

    # -- winreg API ----------------------------------------------------
    def OpenKey(self, root, sub, reserved=0, access=KEY_READ):
        path = self._norm(root, sub)
        self.calls.append(('OpenKey', path))
        self._node(path)  # 不存在则抛 OSError，与真实行为一致
        return path

    def CreateKey(self, root, sub):
        path = self._norm(root, sub)
        self.calls.append(('CreateKey', path))
        self.create_path(path)
        return path

    def CloseKey(self, k):
        self.calls.append(('CloseKey', str(k)))

    def QueryValueEx(self, k, name):
        node = self._node(k)
        if name not in node['values']:
            raise OSError(2, 'The system cannot find the file specified')
        return node['values'][name]

    def SetValueEx(self, k, name, reserved, vtype, value):
        node = self._node(k)
        node['values'][name] = (value, vtype)

    def DeleteValue(self, k, name):
        node = self._node(k)
        if name not in node['values']:
            raise OSError(2, 'The system cannot find the file specified')
        del node['values'][name]

    def EnumKey(self, k, i):
        node = self._node(k)
        names = list(node['subkeys'])
        if i >= len(names):
            raise OSError(22, 'No more data is available')
        return names[i]

    def EnumValue(self, k, i):
        node = self._node(k)
        items = list(node['values'].items())
        if i >= len(items):
            raise OSError(22, 'No more data is available')
        n, (v, t) = items[i]
        return (n, v, t)

    # -- 测试辅助 --------------------------------------------------------
    def remove_path(self, path):
        """整棵删掉一个键（模拟 Run 键不存在的极端情况）"""
        parts = path.lower().split('\\')
        parent = self._node('\\'.join(parts[:-1]) if len(parts) > 1 else '')
        parent['subkeys'].pop(parts[-1], None)

    def get_value(self, path, name):
        """直接读取（测试断言用），不存在返回 None"""
        try:
            return self.QueryValueEx(path.lower(), name)[0]
        except OSError:
            return None

    def set_value(self, path, name, value, vtype=1):
        self.SetValueEx(path.lower(), name, 0, vtype, value)

    def subkey_names(self, path):
        try:
            node = self._node(path.lower())
        except OSError:
            return []
        return list(node['subkeys'])


WINREG = FakeWinreg()
sys.modules['winreg'] = WINREG

# hid 后端：默认枚举不到任何设备
if 'hid' not in sys.modules:
    _hid = types.ModuleType('hid')
    _hid.enumerate = lambda vid, pid: []

    class _HidDevice(object):
        def open_path(self, p):
            raise OSError('no hid device in test sandbox')

        def set_nonblocking(self, v):
            pass

        def close(self):
            pass

    _hid.device = _HidDevice
    sys.modules['hid'] = _hid


def reset():
    """每个测试用例 setUp 时调用：清调用记录、清自定义 handler、重置注册表"""
    del CALLS[:]
    HANDLERS.clear()
    if isinstance(WINREG, FakeWinreg):
        WINREG.reset()


def calls_of(name, dll=None):
    """过滤调用记录：calls_of('AppendMenuW', 'user32')"""
    return [c for c in CALLS if c[1] == name and (dll is None or c[0] == dll)]
