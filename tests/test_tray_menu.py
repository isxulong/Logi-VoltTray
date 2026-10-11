# -*- coding: utf-8 -*-
"""logi_tray 单测：右键菜单构造（含「开机启动」勾选态）、开关切换、
启动时状态同步、托盘认领回退匹配、_refresh 容错与认领重试上限。

差异测试（rebuild 应失败、optimized 应通过）：
  * test_menu_contains_autostart_item —— rebuild 菜单没有开机启动项
  * test_external_delete_not_resurrected —— 以注册表为唯一事实源，外部删除不复活
  * test_claim_fallback_ignores_foreign_tooltip —— rebuild 的宽泛前缀
    （'鼠标'/'键盘'/'MX'）会把其他程序的托盘条目误标 IsPromoted=1
  * test_refresh_tolerates_bad_font_size —— rebuild 遇到损坏的 font_size
    直接抛 ValueError，图标刷新永久中断
"""
import json
import os
import shutil
import tempfile
import unittest
from types import SimpleNamespace

import _winstub

import logi_config as cfg
import logi_tray as T


class TempDirCase(unittest.TestCase):
    def setUp(self):
        _winstub.reset()
        self.tmp = tempfile.mkdtemp(prefix='logitest_')
        self._old = os.environ.get('LOCALAPPDATA')
        os.environ['LOCALAPPDATA'] = self.tmp

    def tearDown(self):
        if self._old is None:
            os.environ.pop('LOCALAPPDATA', None)
        else:
            os.environ['LOCALAPPDATA'] = self._old
        shutil.rmtree(self.tmp, ignore_errors=True)


def menu_calls():
    # AppendMenuW(hmenu, flags, cmd, text) -> 只关心后三个参数
    return [c[2][1:] for c in _winstub.calls_of('AppendMenuW', 'user32')]


class TestMenu(TempDirCase):
    def _popup(self):
        fake = SimpleNamespace(hwnd=77)
        T.TrayApp._menu(fake)
        return menu_calls()

    def test_menu_structure_and_order(self):
        calls = self._popup()
        # 原有三项必须原样保留：重新检测设备 / 生成设备报告 / 分隔线 / 退出
        self.assertEqual(calls[0], (T.MF_STRING | T.MF_DEFAULT,
                                    T.ID_RESCAN, "重新检测设备"))
        self.assertEqual(calls[1], (T.MF_STRING, T.ID_REPORT,
                                    "生成设备报告（排查用）"))
        self.assertEqual(calls[2], (T.MF_SEPARATOR, 0, None))
        self.assertEqual(calls[-1], (T.MF_STRING, T.ID_EXIT, "退出"))

    def test_menu_contains_autostart_item(self):
        calls = self._popup()
        auto = [c for c in calls if len(c) >= 3 and c[1] == T.ID_AUTOSTART]
        self.assertEqual(len(auto), 1, '菜单必须包含「开机启动」项')
        self.assertEqual(auto[0][2], "开机启动")
        self.assertEqual(auto[0][0] & T.MF_CHECKED, 0,
                         '注册表为空时不得勾选')

    def test_menu_checkmark_follows_registry(self):
        _winstub.WINREG.set_value(cfg.RUN_KEY, cfg.APP_NAME,
                                  cfg.expected_run_value())
        calls = self._popup()
        auto = [c for c in calls if len(c) >= 3 and c[1] == T.ID_AUTOSTART]
        self.assertEqual(len(auto), 1)
        self.assertTrue(auto[0][0] & T.MF_CHECKED,
                        '注册表已有自启动项且路径一致时菜单必须勾选（以注册表为准）')

    def test_menu_checkmark_stale_path_not_checked(self):
        """注册表值存在但指向已移动的旧路径 → 判未开启、不勾选"""
        _winstub.WINREG.set_value(cfg.RUN_KEY, cfg.APP_NAME,
                                  '"C:\\Apps\\LogiBatteryPro.exe" --startup')
        calls = self._popup()
        auto = [c for c in calls if len(c) >= 3 and c[1] == T.ID_AUTOSTART]
        self.assertEqual(len(auto), 1)
        self.assertEqual(auto[0][0] & T.MF_CHECKED, 0,
                         '陈旧路径必须判为未开启（路径一致性校验）')


class TestToggleAutostart(TempDirCase):
    def _fake_app(self):
        fake = SimpleNamespace(settings=cfg.load_settings(), tips=[])
        fake._tips_show = lambda text: fake.tips.append(text)
        return fake

    def test_toggle_on_writes_registry_and_settings(self):
        fake = self._fake_app()
        T.TrayApp._toggle_autostart(fake)
        self.assertTrue(cfg.autostart_enabled())
        val = cfg.autostart_value()
        self.assertTrue(val.endswith('" --startup'), val)
        self.assertTrue(fake.settings['autostart'])
        # settings 已持久化
        on_disk = json.load(open(cfg.settings_file(), encoding='utf-8'))
        self.assertTrue(on_disk['autostart'])
        self.assertEqual(fake.tips, ['已开启开机启动'])

    def test_toggle_off_removes_registry_and_settings(self):
        cfg.set_autostart(True)
        fake = self._fake_app()
        T.TrayApp._toggle_autostart(fake)
        self.assertFalse(cfg.autostart_enabled())
        self.assertFalse(fake.settings['autostart'])
        self.assertEqual(fake.tips, ['已关闭开机启动'])

    def test_toggle_failure_shows_message(self):
        # 模拟 onefile 回溯失败：无法得到有效路径 -> 写入被拒绝
        orig = cfg.launcher_path
        cfg.launcher_path = lambda: r'C:\Temp\_MEI1\x.exe'
        try:
            fake = self._fake_app()
            T.TrayApp._toggle_autostart(fake)
        finally:
            cfg.launcher_path = orig
        self.assertFalse(cfg.autostart_enabled())
        self.assertEqual(fake.tips, ['开机启动设置失败，详见 tray.log'])
        self.assertFalse(fake.settings['autostart'],
                         '写入失败时 settings 不得被改成 True')


class TestHealAutostart(TempDirCase):
    """合并版启动对齐：注册表为唯一事实源（外部删除不复活）+ 陈旧路径自愈"""
    FAKE_EXE = r'C:\Apps\LogiBatteryPro.exe'

    def setUp(self):
        super().setUp()
        self._orig_lp = cfg.launcher_path
        cfg.launcher_path = lambda: self.FAKE_EXE

    def tearDown(self):
        cfg.launcher_path = self._orig_lp
        super().tearDown()

    def _write_settings(self, autostart):
        s = dict(cfg.DEFAULTS)
        s['autostart'] = autostart
        cfg.save_settings(s)

    def test_external_delete_not_resurrected(self):
        """settings 里开着、但用户已在系统里删除注册表项 ->
        绝不复活（注册表为唯一事实源），settings 镜像回 False"""
        self._write_settings(True)
        T._heal_autostart()
        self.assertIsNone(cfg.autostart_value(),
                          '外部删除的自启项绝不能被复活')
        self.assertFalse(cfg.autostart_enabled())
        on_disk = json.load(open(cfg.settings_file(), encoding='utf-8'))
        self.assertFalse(on_disk['autostart'],
                         '注册表为真源：settings 必须镜像回 False')

    def test_heal_writeback(self):
        """注册表里有（用户在系统里自己开过）、settings 是关 ->
        以注册表为准回写 settings=True，重启后菜单勾选与注册表一致"""
        self._write_settings(False)
        cfg.set_autostart(True)
        T._heal_autostart()
        on_disk = json.load(open(cfg.settings_file(), encoding='utf-8'))
        self.assertTrue(on_disk['autostart'],
                        '注册表为真时 settings 必须回写为 True')

    def test_heal_stale_path_selfheal(self):
        """注册表值指向已移动的旧路径 -> 按当前程序路径自愈重写（值已存在，非复活）"""
        self._write_settings(True)
        _winstub.WINREG.set_value(cfg.RUN_KEY, cfg.APP_NAME,
                                  '"D:\\old\\LogiBatteryPro.exe" --startup')
        T._heal_autostart()
        self.assertEqual(cfg.autostart_value(),
                         '"%s" --startup' % self.FAKE_EXE,
                         '过期路径必须被自愈重写为当前路径')

    def test_heal_off_stays_off(self):
        self._write_settings(False)
        T._heal_autostart()
        self.assertFalse(cfg.autostart_enabled())


class TestClaimFallback(TempDirCase):
    REG = T._REG_PATH

    def _add_notify_key(self, sub, tooltip):
        _winstub.WINREG.create_path(self.REG + '\\' + sub)
        if tooltip is not None:
            _winstub.WINREG.set_value(self.REG + '\\' + sub,
                                      'InitialTooltip', tooltip)

    def test_claim_fallback_ignores_foreign_tooltip(self):
        """回退匹配不得误伤其他程序的托盘条目"""
        foreign = 'otherapp.exe?a1b2'
        own = 'logibatterypro.exe?c3d4'
        self._add_notify_key(foreign, '鼠标加速器 v2')
        self._add_notify_key(own, 'MX Master 3S 鼠标 85%')

        fake = SimpleNamespace(
            _keys_before={foreign, own},          # 无 fresh
            _used_tips={'MX Master 3S 鼠标 85%'},  # 本程序实际用过的 tooltip
        )
        ok = T.TrayApp._claim_tray(fake)
        self.assertTrue(ok)
        self.assertEqual(
            _winstub.WINREG.get_value(self.REG + '\\' + own, 'IsPromoted'), 1,
            '自己的条目要被固定')
        self.assertIsNone(
            _winstub.WINREG.get_value(self.REG + '\\' + foreign, 'IsPromoted'),
            '其他程序的托盘条目绝不能被改写')

    def test_claim_nothing_to_claim_retries(self):
        fake = SimpleNamespace(_keys_before=set(), _used_tips=set())
        self.assertFalse(T.TrayApp._claim_tray(fake))  # 无可认领 -> 稍后重试


class FakeTrayIcon(object):
    def __init__(self, hwnd, uid):
        self.hwnd = hwnd
        self.uid = uid
        self.updates = []
        self.removed = False

    def update(self, *args):
        self.updates.append(args)

    def remove(self):
        self.removed = True


class RefreshCase(TempDirCase):
    def setUp(self):
        super().setUp()
        self._orig_icon = T.TrayIcon
        T.TrayIcon = FakeTrayIcon

    def tearDown(self):
        T.TrayIcon = self._orig_icon
        super().tearDown()

    def _fake_app(self, snap=None, ready=False, **settings):
        svc = SimpleNamespace(snapshot=lambda: snap or [], ready=ready,
                              request_rescan=lambda: None)
        fake = SimpleNamespace(
            svc=svc, settings=settings, tips={}, hwnd=1, size=32,
            _claimed=False, _claim_at=0.0, _claim_attempts=0,
            _used_tips=set(), _keys_before=set(), _uid=[100])
        fake._next_uid = lambda: fake._uid.__setitem__(0, fake._uid[0] + 1) or fake._uid[0]
        return fake


class TestRefresh(RefreshCase):
    def test_refresh_tolerates_bad_font_size(self):
        """settings 里 font_size 被改坏时刷新不能整个崩掉"""
        fake = self._fake_app(font_size='abc')
        T.TrayApp._refresh(fake)          # 不应抛异常
        self.assertIn('status', fake.tips)
        args = fake.tips['status'].updates[0]
        self.assertEqual(args[5], 0, '损坏的 font_size 要按 0（默认）处理')

    def test_refresh_records_used_tooltips(self):
        snap = [{'name': 'MX Master 3S', 'kind': 'mouse', 'percent': 85,
                 'charging': False, 'plugged': False, 'approx': False,
                 'online': True, 'placeholder': False, 'source': '0x1004',
                 'voltage': None, 'id': 'C52B-1'}]
        fake = self._fake_app(snap=snap, ready=True)
        T.TrayApp._refresh(fake)
        self.assertIn('MX Master 3S 鼠标 85%', fake._used_tips)

    def test_claim_retry_gives_up_after_cap(self):
        """认领重试不能无限进行（每 2 秒一次全量注册表枚举 + 写日志）"""
        fake = self._fake_app()
        for _ in range(120):
            fake._claim_at = 0.0          # 强制每次都进入认领分支
            T.TrayApp._refresh(fake)
        self.assertEqual(fake._claim_attempts, 91,
                         '认领重试必须在 90 次后停止')
        self.assertTrue(fake._claimed)


class TestWndProcDispatch(TempDirCase):
    def test_command_dispatches(self):
        hits = []
        svc = SimpleNamespace(request_rescan=lambda: hits.append('rescan'),
                              stop=lambda wait=True, timeout=10.0: None)
        fake = SimpleNamespace(
            svc=svc, tips={},
            _make_report=lambda: hits.append('report'),
            _toggle_autostart=lambda: hits.append('autostart'))
        wnd = T.TrayApp._wndproc
        wnd(fake, 1, T.WM_COMMAND, T.ID_RESCAN, 0)
        wnd(fake, 1, T.WM_COMMAND, T.ID_REPORT, 0)
        wnd(fake, 1, T.WM_COMMAND, T.ID_AUTOSTART, 0)
        self.assertEqual(hits, ['rescan', 'report', 'autostart'])

    def test_make_report_reentrant_guard(self):
        fake = SimpleNamespace(_reporting=True)
        T.TrayApp._make_report(fake)      # 已在生成中 -> 直接返回
        self.assertTrue(fake._reporting)  # 状态不被打乱


if __name__ == '__main__':
    unittest.main()
