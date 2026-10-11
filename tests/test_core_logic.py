# -*- coding: utf-8 -*-
"""logi_core 纯逻辑单测：设备类型猜测、known.json 迁移/读写、
snapshot 排序、stop(timeout) 语义、_reopen 异常路径的通道释放。

差异测试（rebuild 应失败、optimized 应通过）：
  * test_migration_preserves_name —— rebuild 迁移时把设备名丢成 ''
  * test_reopen_failure_closes_fresh_channels —— rebuild 异常路径泄漏新开的 HID 通道
"""
import json
import os
import shutil
import tempfile
import threading
import time
import unittest
from types import SimpleNamespace

import _winstub  # noqa: F401

import logi_core as C


class TempDirCase(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.mkdtemp(prefix='logitest_')
        self._old_lapp = os.environ.get('LOCALAPPDATA')
        os.environ['LOCALAPPDATA'] = self.tmp

    def tearDown(self):
        if self._old_lapp is None:
            os.environ.pop('LOCALAPPDATA', None)
        else:
            os.environ['LOCALAPPDATA'] = self._old_lapp
        shutil.rmtree(self.tmp, ignore_errors=True)


class TestGuessKind(unittest.TestCase):
    def test_keyboards(self):
        for n in ('MX Keys', 'MX Keys Mini', 'Craft', 'K580', 'ERGO K860',
                  'G915 TKL', 'MX Mechanical'):
            self.assertEqual(C.guess_kind_from_name(n), 'keyboard', n)

    def test_mice(self):
        for n in ('MX Master 3S', 'MX Anywhere 3', 'M350', 'G502 X',
                  'PRO X SUPERLIGHT 2', 'MX Vertical', 'PRO X 2 DEX'):
            self.assertEqual(C.guess_kind_from_name(n), 'mouse', n)

    def test_unknown(self):
        self.assertIsNone(C.guess_kind_from_name('USB Receiver'))
        self.assertIsNone(C.guess_kind_from_name(''))
        self.assertIsNone(C.guess_kind_from_name(None))


class TestCache(TempDirCase):
    def test_roundtrip(self):
        data = {'C52B|tag|1': {'kind': 'mouse', 'name': 'MX Master 3S'}}
        C._save_cache(data)
        self.assertEqual(C._load_cache(), data)

    def test_corrupt_returns_empty(self):
        with open(C._cache_path(), 'w', encoding='utf-8') as f:
            f.write('{not json')
        self.assertEqual(C._load_cache(), {})

    def test_migration_preserves_name(self):
        """旧版 LogiBattery/known.json 迁移时设备名必须保留"""
        olddir = os.path.join(self.tmp, 'LogiBattery')
        os.makedirs(olddir)
        with open(os.path.join(olddir, 'known.json'), 'w', encoding='utf-8') as f:
            json.dump({'C52B|tag|1': {'kind': 'mouse', 'name': 'MX Master 3S'},
                       'C52B|tag|2': {'kind': '', 'name': 'x'}}, f)
        data = C._load_cache()
        self.assertEqual(data.get('C52B|tag|1'),
                         {'kind': 'mouse', 'name': 'MX Master 3S'})
        self.assertNotIn('C52B|tag|2', data)  # kind 为空的脏条目不迁移


def fake_device(name, kind, pid, dev_index):
    return SimpleNamespace(
        name=name, kind=kind, percent=50, charging=False, plugged=False,
        approx=False, online=True, placeholder=False, source='0x1004',
        voltage=None, channel=SimpleNamespace(pid=pid), dev_index=dev_index)


class TestSnapshotOrder(TempDirCase):
    def test_mouse_keyboard_device_order(self):
        svc = C.BatteryService()
        svc.devices = [
            fake_device('未知设备', 'device', 0xC52B, 3),
            fake_device('MX Keys', 'keyboard', 0xC52B, 2),
            fake_device('MX Master 3S', 'mouse', 0xC52B, 1),
        ]
        snap = svc.snapshot()
        self.assertEqual([d['kind'] for d in snap],
                         ['mouse', 'keyboard', 'device'])
        self.assertEqual(snap[0]['id'], 'C52B-1')


class TestStopSemantics(TempDirCase):
    """stop(wait, timeout) 签名（采蓝方案）：默认等待线程退出，wait=False 不阻塞"""

    def test_stop_default_waits_and_joins(self):
        svc = C.BatteryService()
        svc._thread = threading.Thread(
            target=lambda: svc._stop.wait(5.0), daemon=True)
        svc._thread.start()
        t0 = time.time()
        svc.stop()                      # 默认 wait=True：置标志并 join
        self.assertFalse(svc._thread.is_alive())
        self.assertLess(time.time() - t0, 2.0)

    def test_stop_with_explicit_timeout_joins_thread(self):
        svc = C.BatteryService()
        svc._thread = threading.Thread(
            target=lambda: svc._stop.wait(5.0), daemon=True)
        svc._thread.start()
        t0 = time.time()
        svc.stop(wait=True, timeout=2.0)
        self.assertFalse(svc._thread.is_alive())
        self.assertLess(time.time() - t0, 2.0)

    def test_stop_no_wait_returns_immediately(self):
        svc = C.BatteryService()
        svc._thread = threading.Thread(
            target=lambda: svc._stop.wait(5.0), daemon=True)
        svc._thread.start()
        t0 = time.time()
        svc.stop(wait=False)            # 退出路径：只置标志，不 join（不阻塞 UI）
        self.assertLess(time.time() - t0, 0.5)
        self.assertTrue(svc._thread.is_alive())
        svc.stop(wait=True, timeout=2.0)  # 清理
        self.assertFalse(svc._thread.is_alive())


class FakeChannel(object):
    def __init__(self, pid=0xC52B, tag='tag1'):
        self.pid = pid
        self.tag = tag
        self.closed = 0

    def close(self):
        self.closed += 1


class TestReopenChannelLeak(TempDirCase):
    def test_reopen_failure_closes_fresh_channels(self):
        """_reopen 扫描中途抛异常时，新打开的通道必须全部关闭，
        否则 HID 独占句柄泄漏会导致后续再也打不开设备"""
        chans = [FakeChannel(), FakeChannel(0x102A, 'tag2')]
        svc = C.BatteryService()

        orig_open = C.open_channels
        orig_scan = C.BatteryService._scan_channel
        C.open_channels = lambda: list(chans)

        def boom(ch):
            raise RuntimeError('simulated scan failure')

        C.BatteryService._scan_channel = staticmethod(boom)
        try:
            svc._reopen()   # 不应抛出（异常在内部兜底）
        finally:
            C.open_channels = orig_open
            C.BatteryService._scan_channel = orig_scan

        self.assertEqual([c.closed for c in chans], [1, 1],
                         '异常路径必须关闭所有新开的通道')
        self.assertEqual(svc.devices, [])


if __name__ == '__main__':
    unittest.main()
