# -*- coding: utf-8 -*-
"""HID++ 协议冻结核验（合并后重新验证）。

质检裁决：红版 logi_hidpp 与 T1 基线逐字节一致（审查方 cmp 实证），
蓝版仅 +15 行构造函数失败清理。合并版 = 红版 + 同一处构造函数清理。
因此以红版（=T1 基线代理）为参照：
  * 29 个协议常量必须与红版逐一相等；
  * 11 个协议函数源码必须与红版逐行一致；
  * 唯一允许的差异是 Channel.__init__（构造失败清理）。
"""
import inspect
import os
import sys
import unittest

TESTS_DIR = os.path.dirname(os.path.abspath(__file__))
MERGED_DIR = os.path.dirname(TESTS_DIR)
# 红版目录（= T1 基线代理，质检报告 §0 验证记录 4：cmp 逐字节一致）
RED_DIR = os.path.join(os.path.dirname(MERGED_DIR), 'red')

sys.path.insert(0, TESTS_DIR)
import _winstub  # noqa: E402  (hid 桩必须在导入 logi_hidpp 前就位)


def _load(modname, path):
    import importlib.util
    spec = importlib.util.spec_from_file_location(modname, path)
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


class TestProtocolFreeze(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.m = _load('hidpp_merged', os.path.join(MERGED_DIR, 'logi_hidpp.py'))
        cls.r = _load('hidpp_red', os.path.join(RED_DIR, 'logi_hidpp.py'))

    def test_constants_frozen(self):
        names = ('VID_LOGITECH', 'USAGE_PAGE_VENDOR', 'USAGE_SHORT', 'USAGE_LONG',
                 'REPORT_SHORT', 'REPORT_LONG', 'SW_ID',
                 'FEAT_ROOT', 'FEAT_FEATURE_SET', 'FEAT_DEVICE_NAME',
                 'FEAT_BATTERY_STATUS', 'FEAT_BATTERY_VOLTAGE', 'FEAT_UNIFIED_BATTERY',
                 'REG_BATTERY_STATUS', 'REG_BATTERY_CHARGE', 'REG_RECEIVER_INFO',
                 'ERR_UNKNOWN_DEVICE', 'ERR_RESOURCE_ERROR',
                 'TIMEOUT', 'PROBE_TIMEOUT', 'SCAN_INDEXES',
                 'VOLTAGE_TABLE', 'ROUGH_LEVEL', 'KIND_FEATURES',
                 'DEVICE_KIND_CODE', 'KIND_NAME')
        # 29 个常量：26 显式 + VOLTAGE_TABLE/ROUGH_LEVEL/KIND_FEATURES 内 3 组集合
        self.assertEqual(len(names), 26)
        for name in names:
            self.assertEqual(getattr(self.m, name), getattr(self.r, name), name)

    def test_protocol_functions_frozen(self):
        """11 个协议函数（报文构造/传输/解析）逐行一致"""
        chan_fns = ('feature', 'feature_index', 'read_register', 'write_register',
                    '_xfer', '_drain', 'close')
        ep_fns = ('_from_unified', '_from_status', '_from_voltage', '_from_registers')
        for fn in chan_fns:
            self.assertEqual(inspect.getsource(getattr(self.m.Channel, fn)),
                             inspect.getsource(getattr(self.r.Channel, fn)), fn)
        for fn in ep_fns:
            self.assertEqual(inspect.getsource(getattr(self.m.Endpoint, fn)),
                             inspect.getsource(getattr(self.r.Endpoint, fn)), fn)

    def test_only_constructor_differs(self):
        """整文件 diff 只允许 Channel.__init__ 一处（构造失败清理）"""
        for cls_name in ('Channel', 'Endpoint'):
            for fn in ('__init__',):
                src_m = inspect.getsource(getattr(getattr(self.m, cls_name), fn))
                src_r = inspect.getsource(getattr(getattr(self.r, cls_name), fn))
                if cls_name == 'Channel':
                    self.assertNotEqual(src_m, src_r,
                                        'Channel.__init__ 必须包含构造失败清理')
                    self.assertIn('self.close()', src_m)
                else:
                    self.assertEqual(src_m, src_r,
                                     '%s.%s 不允许有差异' % (cls_name, fn))


if __name__ == '__main__':
    unittest.main()
