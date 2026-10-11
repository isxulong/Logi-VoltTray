# -*- coding: utf-8 -*-
"""logi_config 单测：开机启动注册表逻辑（内存 winreg）、settings 读写、
日志限长、launcher_path 句柄管理。

差异测试（rebuild 应失败、optimized 应通过）：
  * test_set_autostart_refuses_mei_path —— rebuild 会把 _MEI 临时路径写进 Run 键
  * test_launcher_path_closes_handle_once —— rebuild 在非 .exe 父进程路径上
    对同一句柄 CloseHandle 两次（double-close）
  * test_log_file_size_capped —— rebuild 日志无限增长
"""
import ctypes
import json
import os
import shutil
import sys
import tempfile
import unittest

import _winstub

import logi_config as cfg


class TempDirCase(unittest.TestCase):
    def setUp(self):
        _winstub.reset()
        self.tmp = tempfile.mkdtemp(prefix='logitest_')
        self._old = {k: os.environ.get(k) for k in ('LOCALAPPDATA', 'TEMP')}
        os.environ['LOCALAPPDATA'] = self.tmp
        os.environ.pop('TEMP', None)

    def tearDown(self):
        for k, v in self._old.items():
            if v is None:
                os.environ.pop(k, None)
            else:
                os.environ[k] = v
        shutil.rmtree(self.tmp, ignore_errors=True)


class TestAutostartRegistry(TempDirCase):
    def test_enable_writes_quoted_path_with_flag(self):
        self.assertTrue(cfg.set_autostart(True))
        val = _winstub.WINREG.get_value(cfg.RUN_KEY, cfg.APP_NAME)
        self.assertIsNotNone(val)
        self.assertTrue(val.startswith('"'), val)
        self.assertTrue(val.endswith('" --startup'), val)
        self.assertIn(sys.executable, val)
        self.assertTrue(cfg.autostart_enabled())
        self.assertEqual(cfg.autostart_value(), val)

    def test_disable_removes_value(self):
        cfg.set_autostart(True)
        self.assertTrue(cfg.set_autostart(False))
        self.assertIsNone(_winstub.WINREG.get_value(cfg.RUN_KEY, cfg.APP_NAME))
        self.assertFalse(cfg.autostart_enabled())

    def test_disable_when_absent_is_ok(self):
        self.assertTrue(cfg.set_autostart(False))
        self.assertFalse(cfg.autostart_enabled())

    def test_set_autostart_refuses_mei_path(self):
        """onefile 回溯失败时禁止把 _MEI 临时副本路径写进注册表
        （重启后该路径已被删除，开机启动会静默失效）"""
        orig = cfg.launcher_path
        cfg.launcher_path = lambda: (r'C:\Users\xl\AppData\Local\Temp'
                                     r'\_MEI12345\LogiBatteryPro.exe')
        try:
            self.assertEqual(cfg.expected_run_value(), '')
            self.assertFalse(cfg.set_autostart(True))
        finally:
            cfg.launcher_path = orig
        self.assertIsNone(_winstub.WINREG.get_value(cfg.RUN_KEY, cfg.APP_NAME))

    def test_expected_run_value_normal(self):
        self.assertEqual(cfg.expected_run_value(),
                         '"%s" --startup' % sys.executable)

    def test_enable_works_when_run_key_absent(self):
        """Run 键整个不存在时也能开启（CreateKey 而非 OpenKey）"""
        _winstub.WINREG.remove_path(cfg.RUN_KEY)
        self.assertTrue(cfg.set_autostart(True))
        creates = [c for c in _winstub.WINREG.calls if c[0] == 'CreateKey']
        self.assertTrue(creates, 'Run 键不存在时必须走 CreateKey')
        self.assertTrue(cfg.autostart_enabled())

    def test_stale_path_counts_as_disabled_and_gets_rewritten(self):
        """注册表值指向别的路径（程序搬家/旧版残留）→ 视为未开启；
        开启时改写成当前程序路径"""
        _winstub.WINREG.set_value(cfg.RUN_KEY, cfg.APP_NAME,
                                  '"C:\\old\\place\\LogiBatteryPro.exe" --startup')
        self.assertFalse(cfg.autostart_enabled(),
                         '陈旧路径必须判为未开启（以注册表+路径一致性为准）')
        self.assertTrue(cfg.set_autostart(True))
        self.assertEqual(_winstub.WINREG.get_value(cfg.RUN_KEY, cfg.APP_NAME),
                         '"%s" --startup' % sys.executable)
        self.assertTrue(cfg.autostart_enabled())

    def test_external_delete_is_respected(self):
        """以注册表为准：开启后用户在系统里手动删项 → 状态回显为关"""
        cfg.set_autostart(True)
        self.assertTrue(cfg.autostart_enabled())
        k = _winstub.WINREG.OpenKey(_winstub.WINREG.HKEY_CURRENT_USER, cfg.RUN_KEY)
        _winstub.WINREG.DeleteValue(k, cfg.APP_NAME)
        self.assertFalse(cfg.autostart_enabled())

    def test_registry_handles_closed(self):
        """每次打开/创建的注册表句柄都有对应关闭"""
        cfg.set_autostart(True)
        cfg.autostart_enabled()
        cfg.toggle_autostart()
        opens = [c for c in _winstub.WINREG.calls if c[0] in ('OpenKey', 'CreateKey')]
        closes = [c for c in _winstub.WINREG.calls if c[0] == 'CloseKey']
        self.assertGreaterEqual(len(closes), len(opens),
                                '注册表句柄泄漏: opens=%d closes=%d'
                                % (len(opens), len(closes)))

    def test_toggle_flips_and_mirrors_settings(self):
        new, ok = cfg.toggle_autostart()
        self.assertTrue(ok)
        self.assertTrue(new)
        self.assertTrue(cfg.autostart_enabled())
        self.assertTrue(cfg.load_settings()['autostart'])  # settings 只做镜像
        new, ok = cfg.toggle_autostart()
        self.assertTrue(ok)
        self.assertFalse(new)
        self.assertFalse(cfg.autostart_enabled())
        self.assertFalse(cfg.load_settings()['autostart'])


class TestRunValueParsing(unittest.TestCase):
    def test_quoted_with_args(self):
        self.assertEqual(cfg._exe_from_run_value('"C:\\a b\\x.exe" --startup'),
                         'C:\\a b\\x.exe')

    def test_unquoted_with_args(self):
        self.assertEqual(cfg._exe_from_run_value('C:\\a\\x.exe --startup'),
                         'C:\\a\\x.exe')

    def test_empty(self):
        self.assertEqual(cfg._exe_from_run_value(''), '')
        self.assertEqual(cfg._exe_from_run_value(None), '')


class TestLauncherPath(TempDirCase):
    """用脚本化的 kernel32 假句柄验证 Toolhelp 回溯流程的句柄管理"""

    ME_EXE = r'C:\Users\xl\AppData\Local\Temp\_MEI99999\LogiBatteryPro.exe'

    def _install_handlers(self, parent_image):
        def proc32_first(h, pe_ref):
            pe = getattr(pe_ref, '_obj', None)
            if pe is None:
                return 0
            pe.th32ProcessID = 4242        # = DEFAULT_RETURNS['GetCurrentProcessId']
            pe.th32ParentProcessID = 777
            return 1

        def query_image(h, flags, buf, size_ref):
            buf.value = parent_image
            return 1

        _winstub.HANDLERS['Process32FirstW'] = proc32_first
        _winstub.HANDLERS['Process32NextW'] = lambda h, pe: 0
        _winstub.HANDLERS['QueryFullProcessImageNameW'] = query_image

    def _close_count(self, handle):
        return sum(1 for c in _winstub.calls_of('CloseHandle', 'kernel32')
                   if c[2] and c[2][0] == handle)

    def test_launcher_path_closes_handle_once(self):
        """父进程路径不是 .exe 时也要恰好 CloseHandle 一次（不得 double-close）"""
        self._install_handlers(r'C:\Windows\System32\svchost')  # 无 .exe 后缀
        orig_exe = sys.executable
        sys.executable = self.ME_EXE
        try:
            out = cfg.launcher_path()
        finally:
            sys.executable = orig_exe
        self.assertEqual(out, self.ME_EXE)        # 拿不到合法父路径则回退自身
        self.assertEqual(self._close_count(0xABC), 1,
                         'OpenProcess 得到的句柄必须恰好关闭一次')

    def test_launcher_path_returns_parent_exe(self):
        self._install_handlers(r'D:\Apps\LogiBatteryPro.exe')
        orig_exe = sys.executable
        sys.executable = self.ME_EXE
        try:
            out = cfg.launcher_path()
        finally:
            sys.executable = orig_exe
        self.assertEqual(out, r'D:\Apps\LogiBatteryPro.exe')
        self.assertEqual(self._close_count(0xABC), 1)

    def test_launcher_path_plain_install(self):
        # 非 onefile（路径无 _MEI）直接返回 sys.executable，不碰 Toolhelp
        self.assertEqual(cfg.launcher_path(), sys.executable)


class TestSettings(TempDirCase):
    def test_roundtrip(self):
        s = cfg.load_settings()
        s['font_size'] = 14
        s['autostart'] = True
        self.assertTrue(cfg.save_settings(s))
        back = cfg.load_settings()
        self.assertEqual(back['font_size'], 14)
        self.assertTrue(back['autostart'])

    def test_corrupt_falls_back_to_defaults(self):
        with open(cfg.settings_file(), 'w', encoding='utf-8') as f:
            f.write('<<<broken>>>')
        s = cfg.load_settings()
        self.assertEqual(s, dict(cfg.DEFAULTS))

    def test_atomic_write_leaves_no_tmp(self):
        cfg.save_settings(dict(cfg.DEFAULTS))
        self.assertFalse(os.path.exists(cfg.settings_file() + '.tmp'))
        self.assertTrue(os.path.exists(cfg.settings_file()))


class TestLogCap(TempDirCase):
    def test_log_file_size_capped(self):
        path = os.path.join(cfg.data_dir(), 'big.log')
        with open(path, 'w', encoding='utf-8') as f:
            f.write('x' * (600 * 1024))      # 预置一个超上限的旧日志
        cfg.log_file('big.log', 'new line')
        size = os.path.getsize(path)
        self.assertLess(size, 200 * 1024,
                        '日志超过 512KB 后必须截断到只留末尾 128KB')
        with open(path, 'rb') as f:
            head = f.read(64)
        self.assertIn(b'truncated', head)


class TestDataDirFallback(TempDirCase):
    def test_falls_back_to_tempdir(self):
        os.environ.pop('LOCALAPPDATA', None)
        os.environ.pop('TEMP', None)
        d = cfg.data_dir()
        self.assertTrue(d.startswith(tempfile.gettempdir()), d)
        self.assertTrue(os.path.isdir(d))


if __name__ == '__main__':
    unittest.main()
