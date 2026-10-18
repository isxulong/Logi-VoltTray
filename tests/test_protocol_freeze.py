# -*- coding: utf-8 -*-
"""HID++ 协议冻结核验（合并后重新验证）。

质检裁决：蓝版仅 +15 行构造函数失败清理。合并版 = 红版 + 同一处
构造函数清理。因此以红版（=T1 基线代理）为参照：
  * 29 个协议常量必须与红版逐一相等；
  * 11 个协议函数源码必须与红版逐行一致；
  * 唯一允许的差异是 Channel.__init__（构造失败清理）。
v0.3.17（一轮 P0-④）：red/ 基线为「合并版逆推重建」（T1 原件不可考，
终审勘误：质检引用 commit 359b27a 实为 ea9b672，属报告层笔误、仓库
无修复对象）；构造差异断言相应改为结构性（difflib），不再依赖与
不可考原件的逐字节关系。
"""
import inspect
import os
import sys
import unittest

TESTS_DIR = os.path.dirname(os.path.abspath(__file__))
MERGED_DIR = os.path.dirname(TESTS_DIR)
# 红版目录（= T1 基线代理）。v0.3.17（一轮 P0-④）修复：此前指向
# 仓库上级目录的 red/——父目录下并不存在该目录，门禁常年
# FileNotFoundError；现指向库内 red/（基线已逆推重建入库）。
RED_DIR = os.path.join(MERGED_DIR, 'red')

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
        """整文件结构性 diff 只允许 Channel.__init__ 构造失败清理一处。

        v0.3.17（一轮 P0-④）：red/ 为逆推重建基线（T1 原件不可考），
        断言口径从「与原件逐字节比对」改为结构性——整文件 diff 的
        非 equal 块恰好 1 个，且增删行全部落在构造清理语义内。
        """
        import difflib
        with open(os.path.join(MERGED_DIR, 'logi_hidpp.py'), encoding='utf-8') as f:
            m_lines = f.read().splitlines()
        with open(os.path.join(RED_DIR, 'logi_hidpp.py'), encoding='utf-8') as f:
            r_lines = f.read().splitlines()
        sm = difflib.SequenceMatcher(None, r_lines, m_lines)
        ops = [op for op in sm.get_opcodes() if op[0] != 'equal']
        self.assertEqual(len(ops), 1, '只允许 Channel.__init__ 一处差异: %r' % (ops,))
        _tag, i1, i2, j1, j2 = ops[0]
        removed = r_lines[i1:i2]
        added = m_lines[j1:j2]
        joined_add = '\n'.join(added)
        # 新增侧必须构成完整的构造失败清理语义
        for needle in ('try:', 'except Exception:', 'self.close()', 'raise'):
            self.assertIn(needle, joined_add)
        # 删除侧只允许「被 try 包裹前的未缩进函数体」——其内容须逐行
        # 出现在新增侧（仅缩进不同），即无逻辑改动
        added_stripped = [a.strip() for a in added]
        for line in removed:
            self.assertIn(line.strip(), added_stripped)
        # Endpoint.__init__ 不允许有差异
        src_m = inspect.getsource(self.m.Endpoint.__init__)
        src_r = inspect.getsource(self.r.Endpoint.__init__)
        self.assertEqual(src_m, src_r)


if __name__ == '__main__':
    unittest.main()
