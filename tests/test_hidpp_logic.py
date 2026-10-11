# -*- coding: utf-8 -*-
"""logi_hidpp 纯逻辑单测：电压换算、百分比钳制、名字清洗、路径标签、
HID++ 应答匹配（含 dev^0xFF、错误应答、sub/addr 过滤、超时）、
寄存器寻址映射、Endpoint 各电量解析路径。

这些测试在 rebuild（T1 基线）与 optimized（红方优化版）上应同样通过——
协议语义被冻结，优化不得改变它们。
"""
import threading
import time
import unittest

import _winstub  # noqa: F401  必须先于产品模块打桩

import logi_hidpp as H


class TestVoltageTable(unittest.TestCase):
    def test_boundaries(self):
        self.assertEqual(H.voltage_to_percent(None), None)
        self.assertEqual(H.voltage_to_percent(5000), 100)
        self.assertEqual(H.voltage_to_percent(4186), 100)   # 表顶
        self.assertEqual(H.voltage_to_percent(3500), 0)     # 表底
        self.assertEqual(H.voltage_to_percent(3000), 0)
        self.assertEqual(H.voltage_to_percent(3646), 5)
        self.assertEqual(H.voltage_to_percent(3579), 2)

    def test_interpolation(self):
        # 4019 介于 4067(90) 与 3989(80) 之间：
        # 80 + (90-80)*(4019-3989)/(4067-3989) = 80 + 10*30/78 = 83.846 -> 84
        self.assertEqual(H.voltage_to_percent(4019), 84)
        # 正好落在表内节点
        self.assertEqual(H.voltage_to_percent(3989), 80)
        self.assertEqual(H.voltage_to_percent(3811), 50)

    def test_monotonic(self):
        prev = -1
        for mv in range(3500, 4200, 7):
            p = H.voltage_to_percent(mv)
            self.assertGreaterEqual(p, prev)
            prev = p


class TestClampPct(unittest.TestCase):
    def test_values(self):
        self.assertIsNone(H._clamp_pct(None))
        self.assertIsNone(H._clamp_pct(-1))
        self.assertEqual(H._clamp_pct(0), 0)
        self.assertEqual(H._clamp_pct(55), 55)
        self.assertEqual(H._clamp_pct(100), 100)
        self.assertEqual(H._clamp_pct(101), 100)
        self.assertEqual(H._clamp_pct(255), 100)
        self.assertEqual(H._clamp_pct('42'), 42)


class TestCleanName(unittest.TestCase):
    def test_bytes_with_garbage(self):
        self.assertEqual(H._clean_name(b'MX Master 3S\x00\xff'), 'MX Master 3S')
        self.assertEqual(H._clean_name(b'MX Keys\x00\x00\x00'), 'MX Keys')

    def test_control_chars_removed(self):
        self.assertEqual(H._clean_name('K\x01k580\r\n'), 'Kk580')
        self.assertEqual(H._clean_name('  G502  '), 'G502')
        self.assertEqual(H._clean_name(None), '')
        self.assertEqual(H._clean_name(b''), '')


class TestPathTag(unittest.TestCase):
    def test_collection_suffix_stripped(self):
        a = r'\\?\HID#VID_046D&PID_C52B&MI_02&Col01#8&1234#{4d1e55b2}'
        b = r'\\?\HID#VID_046D&PID_C52B&MI_02&Col02#8&1234#{4d1e55b2}'
        self.assertEqual(H._path_tag(a), H._path_tag(b))
        self.assertNotIn('Col', H._path_tag(a))

    def test_bytes_path(self):
        p = b'\\\\?\\HID#VID_046D&PID_C52B&MI_00&Col01#9&abcd#{x}'
        self.assertEqual(H._path_tag(p), 'VID_046D&PID_C52B&MI_00')


class FakeHID(object):
    """假 HID 句柄：write 记录，read 按脚本出包"""

    def __init__(self, reads=()):
        self.reads = [list(r) for r in reads]
        self.writes = []

    def write(self, buf):
        self.writes.append(list(buf))
        return len(buf)

    def read(self, n):
        # 真实设备只在收到请求后才回包；未 write 前返回空
        # （否则脚本化的应答会在 _xfer 开头的 _drain 阶段被清掉）
        if not self.writes:
            return []
        for i, r in enumerate(self.reads):
            if len(r) == n:
                return self.reads.pop(i)
        return []


def make_channel(short_reads=(), long_reads=()):
    """绕过 hid.device() 构造 Channel（只装配 _xfer 需要的字段）"""
    ch = object.__new__(H.Channel)
    ch.lock = threading.RLock()
    ch.last_err = None
    ch.short = FakeHID(short_reads)
    ch.long = FakeHID(long_reads)
    return ch


class TestXferMatching(unittest.TestCase):
    def test_register_address_mapping(self):
        # reg 0x07 -> sub 0x81, addr 0x07；reg 0x2B5 -> sub 0x83, addr 0xB5
        ch = make_channel(short_reads=[[16, 1, 0x81, 0x07, 9, 9, 9]])
        r = ch.read_register(1, 0x07, timeout=0.1)
        self.assertEqual(ch.short.writes[0][:4], [H.REPORT_SHORT, 1, 0x81, 0x07])
        self.assertEqual(r[:3], [9, 9, 9])

        ch = make_channel(long_reads=[[17, 1, 0x83, 0xB5, 1, 2, 3] + [0] * 13])
        r = ch.read_register(1, 0x2B5, timeout=0.1)
        self.assertEqual(ch.long.writes[0][:4], [H.REPORT_LONG, 1, 0x83, 0xB5])
        self.assertEqual(r[:3], [1, 2, 3])

    def test_bt_reply_dev_xor_255(self):
        # 蓝牙设备可能用 dev^0xFF 回包，必须能匹配
        ch = make_channel(short_reads=[[16, 255 ^ 1, 0x81, 0x0D, 55, 0, 0]])
        r = ch.read_register(1, 0x0D, timeout=0.1)
        self.assertIsNotNone(r)
        self.assertEqual(r[0], 55)

    def test_error_reply(self):
        # 设备休眠：接收器回 10 <dev> 8F <sub> <addr> <err>
        ch = make_channel(short_reads=[[16, 1, 0x8F, 0x81, 0x0D, 8, 0]])
        r = ch.read_register(1, 0x0D, timeout=0.2)
        self.assertIsNone(r)                      # read_register 把 _Err 转为 None
        self.assertIsInstance(ch.last_err, H._Err)
        self.assertEqual(ch.last_err.code, 8)     # ERR_UNKNOWN_DEVICE

    def test_unrelated_reports_filtered(self):
        # 先到一个 sub/addr 不匹配的（设备主动推送），随后才是正确应答
        noise = [16, 1, 0x41, 0x00, 1, 1, 1]
        good = [16, 1, 0x81, 0x0D, 66, 0, 0]
        ch = make_channel(short_reads=[noise, good])
        r = ch.read_register(1, 0x0D, timeout=0.3)
        self.assertIsNotNone(r)
        self.assertEqual(r[0], 66)

    def test_timeout_returns_none(self):
        ch = make_channel()
        t0 = time.time()
        r = ch.read_register(1, 0x0D, timeout=0.05)
        self.assertIsNone(r)
        self.assertLess(time.time() - t0, 2.0)


class FakeChannel(object):
    """Endpoint 层的假通道：按 (kind, ...) 脚本回复"""

    pid = 0xC52B
    tag = 'VID_046D&PID_C52B&MI_02'

    def __init__(self, replies=None):
        self.replies = replies or {}
        self.calls = []

    def feature(self, dev, idx, func, params=(), timeout=H.TIMEOUT):
        self.calls.append(('feature', dev, idx, func, tuple(params)))
        return self.replies.get(('feature', idx, func))

    def feature_index(self, dev, fid, timeout=H.TIMEOUT):
        self.calls.append(('feature_index', dev, fid))
        return self.replies.get(('fidx', fid), 0)

    def read_register(self, dev, reg, params=(), timeout=H.TIMEOUT):
        self.calls.append(('read_register', dev, reg, tuple(params)))
        return self.replies.get(('reg', reg))


def make_endpoint(replies):
    return H.Endpoint(FakeChannel(replies), 1)


class TestEndpointBatteryPaths(unittest.TestCase):
    def test_unified_battery_0x1004(self):
        ep = make_endpoint({('feature', 5, 1): [85, 0, 1, 0]})
        ep._battery_feat = (H.FEAT_UNIFIED_BATTERY, 5)
        self.assertTrue(ep.refresh())
        self.assertEqual(ep.percent, 85)
        self.assertTrue(ep.charging)
        self.assertTrue(ep.plugged)
        self.assertFalse(ep.approx)
        self.assertEqual(ep.source, '0x1004')
        self.assertTrue(ep.online)

    def test_battery_status_0x1000(self):
        ep = make_endpoint({('feature', 4, 0): [30, 20, 3]})
        ep._battery_feat = (H.FEAT_BATTERY_STATUS, 4)
        self.assertTrue(ep.refresh())
        self.assertEqual(ep.percent, 30)
        self.assertTrue(ep.plugged)      # status 3 = 接电源未充电
        self.assertFalse(ep.charging)

    def test_battery_voltage_0x1001(self):
        mv = 4019
        ep = make_endpoint({('feature', 6, 0): [mv >> 8, mv & 255, 0]})
        ep._battery_feat = (H.FEAT_BATTERY_VOLTAGE, 6)
        self.assertTrue(ep.refresh())
        self.assertEqual(ep.voltage, mv)
        self.assertEqual(ep.percent, H.voltage_to_percent(mv))
        self.assertTrue(ep.approx)       # 电压路径必须标“约”
        self.assertFalse(ep.charging)

    def test_register_0x0d_real_percent(self):
        ep = make_endpoint({('reg', H.REG_BATTERY_CHARGE): [66, 0, 0]})
        self.assertTrue(ep._from_registers())
        self.assertEqual(ep.percent, 66)
        self.assertFalse(ep.approx)
        self.assertEqual(ep.source, 'REG 0x0D')

    def test_register_0x0d_scale_255(self):
        # 个别设备 0x0D 回 0~255 原始值，需换算
        ep = make_endpoint({('reg', H.REG_BATTERY_CHARGE): [200, 0, 0]})
        self.assertTrue(ep._from_registers())
        self.assertEqual(ep.percent, round(200 * 100.0 / 255.0))  # 78

    def test_register_0x0d_full_on_ac(self):
        # 状态字节 0x90（144）= 接电源且视为满电：percent 0 时要修正为 100
        ep = make_endpoint({('reg', H.REG_BATTERY_CHARGE): [0, 0, 144]})
        self.assertTrue(ep._from_registers())
        self.assertEqual(ep.percent, 100)
        self.assertTrue(ep.plugged)
        self.assertFalse(ep.charging)

    def test_register_0x07_rough_level(self):
        # 0x0D 不可用，退回 0x07 粗档位：level 7 -> 约 90%，34 -> 接电源未充电
        ep = make_endpoint({
            ('reg', H.REG_BATTERY_CHARGE): None,
            ('reg', H.REG_BATTERY_STATUS): [7, 34],
        })
        self.assertTrue(ep._from_registers())
        self.assertEqual(ep.percent, 90)
        self.assertTrue(ep.approx)
        self.assertTrue(ep.plugged)
        self.assertFalse(ep.charging)
        self.assertEqual(ep.source, 'REG 0x07')

    def test_register_0x07_charging(self):
        ep = make_endpoint({
            ('reg', H.REG_BATTERY_CHARGE): None,
            ('reg', H.REG_BATTERY_STATUS): [5, 33],
        })
        self.assertTrue(ep._from_registers())
        self.assertEqual(ep.percent, 50)
        self.assertTrue(ep.charging)
        self.assertTrue(ep.plugged)

    def test_all_paths_fail_offline(self):
        ep = make_endpoint({})
        self.assertFalse(ep.refresh())
        self.assertFalse(ep.online)      # 失败时 online 必须为 False（对称性）


class TestReceiverDeviceName(unittest.TestCase):
    def test_unifying_subregister(self):
        # dev 2 -> 子寄存器 0x40 + (2-1) = 0x41
        raw = list(b'MX Keys\x00') + [0] * 9
        ch = FakeChannel({('reg', H.REG_RECEIVER_INFO): raw})
        name = H.receiver_device_name(ch, 2)
        self.assertEqual(name, 'MX Keys')
        self.assertIn(('read_register', 255, H.REG_RECEIVER_INFO, (0x41,)), ch.calls)

    def test_bolt_fallback(self):
        # 普通子寄存器读不到时，退回 Bolt 的 0x60+dev, 0x01
        raw = list(b'Bolt Mouse\x00') + [0] * 6

        class BoltChannel(FakeChannel):
            def read_register(self, dev, reg, params=(), timeout=H.TIMEOUT):
                self.calls.append(('read_register', dev, reg, tuple(params)))
                if tuple(params) == (0x63, 1):   # dev 3 -> 0x60+3
                    return raw
                return None

        ch = BoltChannel()
        self.assertEqual(H.receiver_device_name(ch, 3), 'Bolt Mouse')

    def test_out_of_range(self):
        ch = FakeChannel()
        self.assertEqual(H.receiver_device_name(ch, 0), '')
        self.assertEqual(H.receiver_device_name(ch, 7), '')


if __name__ == '__main__':
    unittest.main()
