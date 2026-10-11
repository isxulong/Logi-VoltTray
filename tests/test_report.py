# -*- coding: utf-8 -*-
"""logi_report 单测：hidapi 不可用时的降级行为、报告骨架、服务停止语义。

差异测试（rebuild 应失败、optimized 应通过）：
  * test_collect_without_hid_module —— rebuild 顶部裸 import hid，
    hid 缺失时整个报告功能直接崩
"""
import os
import shutil
import tempfile
import unittest

import _winstub  # noqa: F401

import logi_core
import logi_report as R


class FakeSvc(object):
    """秒回的假 BatteryService（避免 collect 里 8 秒轮询）"""
    stopped_with = None

    def __init__(self, interval=None):
        pass

    def start(self):
        pass

    def snapshot(self):
        return []

    def stop(self, timeout=None):
        FakeSvc.stopped_with = timeout


class TempDirCase(unittest.TestCase):
    def setUp(self):
        _winstub.reset()
        self.tmp = tempfile.mkdtemp(prefix='logitest_')
        self._old = os.environ.get('LOCALAPPDATA')
        os.environ['LOCALAPPDATA'] = self.tmp
        self._orig_svc = logi_core.BatteryService
        logi_core.BatteryService = FakeSvc

    def tearDown(self):
        logi_core.BatteryService = self._orig_svc
        if self._old is None:
            os.environ.pop('LOCALAPPDATA', None)
        else:
            os.environ['LOCALAPPDATA'] = self._old
        shutil.rmtree(self.tmp, ignore_errors=True)


class TestCollect(TempDirCase):
    def test_collect_skeleton_no_devices(self):
        text = R.collect()
        self.assertIn('Logi VoltTray', text)
        self.assertIn('没有枚举到任何罗技设备', text)
        self.assertEqual(FakeSvc.stopped_with, 5.0,
                         '报告采集结束必须带 timeout 停止服务（等 HID 通道释放）')

    def test_collect_without_hid_module(self):
        """hidapi 不可用时报告要给出说明而不是崩溃"""
        orig = R.hid
        R.hid = None
        try:
            text = R.collect()
        finally:
            R.hid = orig
        self.assertIn('hidapi 模块不可用', text)
        self.assertIn('HID++ 通道 0 个', text)


if __name__ == '__main__':
    unittest.main()
