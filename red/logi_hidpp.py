# -*- coding: utf-8 -*-
"""
罗技电量 · 通用版 HID++ 读取核心

在旧版（只认 HID++ 2.0 的 0x1004/0x1000/0x1001 三个特征）基础上，补齐了
老款键鼠、蓝牙直连设备、有线设备的读取路径，目标是覆盖罗技全系列键盘鼠标。

与旧版的差异（新增能力）
--------------------------------------------------------------------------
1. HID++ 1.0 寄存器（旧版完全没有）
     * 0x0D BATTERY_CHARGE  -> 真实电量百分比（首选）
     * 0x07 BATTERY_STATUS  -> 粗略档位（满/良好/低/极低），作为兜底
   老款无线键鼠（K270 / M185 / MK270 / K400 / M345 这类）不支持 HID++ 2.0
   的电量特征，只能靠这两个寄存器，这就是"网友说用不了"的主因。

2. 蓝牙 / USB 线直连设备
   这类设备不经过接收器，直接以设备索引 0xFF 应答。旧版只扫接收器的 1~6，
   所以蓝牙连接的键鼠一个都看不到。
   （罗技蓝牙设备可能用 0x00 回包，因此应答匹配放宽为 dev / dev^0xFF。）

3. 真实设备名
   HID++ 2.0 的 0x0005 DEVICE_NAME（function 0 读长度，function 1 读分片）。
   老设备没有该特征时，退回读接收器的 0x2B5 寄存器（子寄存器 0x4N / 0x6N01）。

4. 设备类型
   优先用 0x0005 的 function 2（HID++ 规范里就定义了设备类型，比按特征猜准），
   拿不到再退回"按特征探测"的老办法。

5. 充电状态、粗略标记
   各协议路径都解析充电状态；只能拿到粗档位时置 approx=True，
   界面会显示成"约 90%"而不是假装是精确值。

协议要点（实测 + Solaar 参考实现核对）
--------------------------------------------------------------------------
* 报文：短 [0x10, dev, sub, addr, p0..p2]（7 字节）
        长 [0x11, dev, sub, addr, p0..p15]（20 字节）
* HID++ 2.0 特征调用：sub = 特征索引, addr = (function << 4) | sw_id
* HID++ 1.0 寄存器读：request = 0x8100 | reg -> sub = request >> 8, addr = request & 0xFF
  （reg 0x07 -> sub 0x81 addr 0x07；reg 0x2B5 -> sub 0x83 addr 0xB5）
* 设备不在线时接收器立刻回 10 <dev> 8F <sub> <addr> 08/09 00，
  可据此快速判定"休眠"，不必等超时。
* addr 必须完整回显才算匹配（与 Solaar 的匹配方式一致），可避免把
  设备主动推送的通知当成应答。
"""

import re
import threading
import time

try:
    import hid
except Exception:
    hid = None


VID_LOGITECH = 1133
USAGE_PAGE_VENDOR = 65280
USAGE_SHORT = 1
USAGE_LONG = 2

REPORT_SHORT = 16
REPORT_LONG = 17

SW_ID = 1

FEAT_ROOT = 0
FEAT_FEATURE_SET = 1
FEAT_DEVICE_NAME = 5
FEAT_BATTERY_STATUS = 4096
FEAT_BATTERY_VOLTAGE = 4097
FEAT_UNIFIED_BATTERY = 4100

REG_BATTERY_STATUS = 7
REG_BATTERY_CHARGE = 13
REG_RECEIVER_INFO = 693

ERR_UNKNOWN_DEVICE = 8
ERR_RESOURCE_ERROR = 9

TIMEOUT = 0.4
PROBE_TIMEOUT = 0.3

SCAN_INDEXES = (1, 2, 3, 4, 5, 6, 255)

KIND_NAME = {'mouse': '鼠标', 'keyboard': '键盘', 'device': '设备', 'receiver': '接收器'}


DEVICE_KIND_CODE = {
    0: 'device', 1: 'keyboard', 2: 'mouse', 3: 'keyboard',
    4: 'device', 7: 'device', 8: 'mouse', 9: 'mouse',
    10: 'device', 11: 'device', 12: 'device', 13: 'device',
    14: 'device', 15: 'receiver',
}





KIND_FEATURES = ((8706, 'mouse'), (6916, 'keyboard'), (8705, 'mouse'), (17697, 'keyboard'), (8704, 'mouse'), (6530, 'keyboard'), (8480, 'mouse'), (17728, 'keyboard'), (8481, 'mouse'), (6531, 'keyboard'))













VOLTAGE_TABLE = ((4186, 100), (4067, 90), (3989, 80), (3922, 70), (3859, 60), (3811, 50), (3778, 40), (3751, 30), (3717, 20), (3671, 10), (3646, 5), (3579, 2), (3500, 0))






ROUGH_LEVEL = {7: 90, 5: 50, 3: 20, 1: 5}


_NAME_OK = re.compile('^[A-Za-z0-9][A-Za-z0-9 _\\-\\.\\+/()]{1,31}$')


def voltage_to_percent(mv):
    if mv is None:
        return None
    if mv >= VOLTAGE_TABLE[0][0]:
        return 100
    if mv <= VOLTAGE_TABLE[-1][0]:
        return 0
    for (v1, p1), (v2, p2) in zip(VOLTAGE_TABLE, VOLTAGE_TABLE[1:]):
        if v2 <= mv <= v1:
            return int(round(p2 + (p1 - p2) * (mv - v2) / float(v1 - v2)))
    return


class _Err(object):
    """HID++ 错误应答（设备休眠 / 特征不支持）"""
    __slots__ = ('code',)

    def __init__(self, code):
        self.code = code

    def __repr__(self):
        return '<Err 0x%02X>' % (self.code if self.code is not None else 0)


def _path_tag(path):
    """从 HID 路径提取设备标识，忽略 collection 后缀（短/长接口要配对成同一个通道）"""
    if isinstance(path, bytes):
        path = path.decode('utf-8', 'replace')
    seg = path.split('#')
    tag = seg[1] if len(seg) > 1 else path
    return re.sub('&Col\\d+', '', tag)



class Channel(object):
    """一个 HID++ 通道：USB 接收器 / 蓝牙设备 / 有线设备。
短报文口（7 字节）和长报文口（20 字节）是两个独立的 HID collection，必须分别打开。
"""

    def __init__(self, pid, tag, short_path, long_path, serial=''):
        self.pid = pid
        self.tag = tag
        self.serial = serial or ''
        self.has_long = long_path is not None
        self.lock = threading.RLock()
        self.last_err = None

        self.short = None
        self.long = None
        self.short = hid.device()
        self.short.open_path(short_path or long_path)
        self.short.set_nonblocking(1)
        if long_path:
            self.long = hid.device()
            self.long.open_path(long_path)
            self.long.set_nonblocking(1)


    def close(self):
        for h in (self.short, self.long):
            if h is None:
                continue
            try:
                h.close()
            except Exception:
                continue
        self.short = self.long = None

    @property
    def name(self):
        return '%04X·%s' % (self.pid, self.tag[-16:])

    def _drain(self):
        for h, size in ((self.long, 20), (self.short, 7)):
            if h is None:
                continue
            for _ in range(80):
                try:
                    if not h.read(size):
                        break
                except Exception:
                    break

    def _xfer(self, dev, sub, addr, params=(), long_msg=True, timeout=TIMEOUT):
        """发一条请求并等匹配的应答。

返回 参数列表（不含报文头）/ _Err（设备回了错误）/ None（超时）
"""
        with self.lock:
            self.last_err = None
            self._drain()

            if long_msg and self.long is not None:
                buf = [REPORT_LONG, dev, sub, addr] + list(params)
                buf += [0] * (20 - len(buf))
                out, size, writer = buf[:20], 20, self.long
            else:
                buf = [REPORT_SHORT, dev, sub, addr] + list(params)
                buf += [0] * (7 - len(buf))
                out, size, writer = buf[:7], 7, self.short

            try:
                if writer.write(out) is None:
                    return None
            except Exception:
                return None

            deadline = time.time() + timeout
            while time.time() < deadline:
                for h, n in ((self.long, 20), (self.short, 7)):
                    if h is None:
                        continue
                    try:
                        r = h.read(n)
                    except Exception:
                        continue
                    if not r or len(r) < 5:
                        continue

                    if r[1] != dev and r[1] != dev ^ 255:
                        continue
                    if r[2] == 143:
                        self.last_err = _Err(r[5] if len(r) > 5 else None)
                        return self.last_err
                    if r[2] == 255:
                        self.last_err = _Err(r[5] if len(r) > 5 else None)
                        return self.last_err
                    if r[2] != sub or r[3] != addr:
                        continue
                    return list(r[4:])
                time.sleep(0.004)
            return None


    def feature(self, dev, feat_idx, func, params=(), timeout=TIMEOUT):
        """HID++ 2.0 特征调用"""
        addr = (func & 15) << 4 | SW_ID & 15
        r = self._xfer(dev, feat_idx, addr, params, long_msg=True, timeout=timeout)
        return None if isinstance(r, _Err) else r

    def feature_index(self, dev, feature_id, timeout=TIMEOUT):
        """查询特征索引（0 表示设备不支持该特征）"""
        p = self.feature(dev, 0, 0,
                        [feature_id >> 8 & 255, feature_id & 255], timeout=timeout)
        if p and len(p) >= 1 and p[0]:
            return p[0]
        return 0

    def read_register(self, dev, reg, params=(), timeout=TIMEOUT):
        """HID++ 1.0 寄存器读。reg 0x07 -> sub 0x81/addr 0x07，0x2B5 -> sub 0x83/addr 0xB5"""
        req = 0x8100 | reg & 0x2FF
        sub, addr = req >> 8 & 255, req & 255

        need_long = reg > 255 and self.long is not None
        r = self._xfer(dev, sub, addr, params, long_msg=need_long, timeout=timeout)
        return None if isinstance(r, _Err) else r

    def write_register(self, dev, reg, params=()):
        req = 0x8000 | reg & 0x2FF
        sub, addr = req >> 8 & 255, req & 255
        need_long = reg > 255 and self.long is not None
        r = self._xfer(dev, sub, addr, params, long_msg=need_long)
        return not isinstance(r, _Err) and r is not None



class Endpoint(object):
    """通道上的一个可寻址设备（接收器配对槽位 / 蓝牙或直连设备）"""

    def __init__(self, channel, dev_index):
        self.channel = channel
        self.dev_index = dev_index

        self.kind = 'device'
        self.name = ''
        self.percent = None
        self.charging = False
        self.plugged = False
        self.approx = False
        self.voltage = None
        self.source = ''
        self.online = False
        self.placeholder = False

        self._hidpp2 = None
        self._feat = {}
        self._battery_feat = None


    @property
    def key(self):
        return (self.channel.pid, self.channel.tag, self.dev_index)

    def _fidx(self, fid, timeout=TIMEOUT):
        if fid not in self._feat:
            self._feat[fid] = self.channel.feature_index(self.dev_index, fid, timeout)
        return self._feat[fid]

    def probe(self):
        """设备是否在线（HID++2 或 HID++1.0 任一能应答即在）"""
        if self._hidpp2 is None:
            self._hidpp2 = bool(self._fidx(FEAT_FEATURE_SET, PROBE_TIMEOUT))
        if self._hidpp2:
            return True
        for reg in (REG_BATTERY_CHARGE, REG_BATTERY_STATUS):
            if self.channel.read_register(self.dev_index, reg, timeout=PROBE_TIMEOUT) is not None:
                return True
        return False


    def read_name(self):
        """HID++2 的 0x0005 DEVICE_NAME；拿不到就回落到接收器的 0x2B5"""
        idx = self._fidx(FEAT_DEVICE_NAME)
        if idx:
            n = self.channel.feature(self.dev_index, idx, 0)
            total = n[0] if n else 0
            if 0 < total <= 64:
                out = b''
                while len(out) < total:
                    frag = self.channel.feature(self.dev_index, idx, 1, [len(out)])
                    if not frag:
                        break
                    piece = bytes(b for b in frag if b != 0)
                    if not piece:
                        break
                    out += piece[:total - len(out)]
                    if len(piece) < len(frag):
                        break
                name = _clean_name(out)
                if name and any(ch.isalnum() for ch in name):
                    return name
        if not self._hidpp2:
            name = receiver_device_name(self.channel, self.dev_index)
            if name:
                return name
        return ''


    def read_kind(self):
        """先用"设备专有特征"反推（旧版实测可靠），
再退回 0x0005 function 2 的设备类型码。

注意：0x0005 fn2 并不可靠——实测罗技 PRO X 2 DEX 鼠标回的是 0x03(numpad)，
所以不能让它优先，只能当兜底。
"""
        for fid, kind in KIND_FEATURES:
            if self._fidx(fid):
                return kind
        idx = self._fidx(FEAT_DEVICE_NAME)
        if idx:
            r = self.channel.feature(self.dev_index, idx, 2)
            if r and len(r) >= 1:
                k = DEVICE_KIND_CODE.get(r[0])
                if k and k != 'receiver':
                    return k
        return 'device'


    def _pick_battery_feature(self):
        for fid in (FEAT_UNIFIED_BATTERY, FEAT_BATTERY_STATUS, FEAT_BATTERY_VOLTAGE):
            idx = self._fidx(fid)
            if idx:
                return (fid, idx)
        return None

    def identify(self):
        """识别设备：类型 + 名称 + 选一条能用的电量通路。不在线返回 False"""
        if not self.probe():
            self.online = False
            return False

        self._battery_feat = self._pick_battery_feature() if self._hidpp2 else None

        if self._hidpp2:
            self.kind = self.read_kind()
            self.name = self.read_name()
        if self._battery_feat is None and not self._hidpp2:
            self.kind = 'device'
            self.name = receiver_device_name(self.channel, self.dev_index) or ''
        if self.kind == 'receiver':
            return False
        self.name = self.name or KIND_NAME.get(self.kind, KIND_NAME['device'])
        self.placeholder = False
        return True

    def refresh(self):
        ok = False
        if self._battery_feat:
            fid, idx = self._battery_feat
            if fid == FEAT_UNIFIED_BATTERY:
                ok = self._from_unified(idx)
            elif fid == FEAT_BATTERY_STATUS:
                ok = self._from_status(idx)
            elif fid == FEAT_BATTERY_VOLTAGE:
                ok = self._from_voltage(idx)
        if not ok:

            ok = self._from_registers()
        self.online = ok
        return ok

    def _from_unified(self, idx):
        """0x1004 UNIFIED_BATTERY：fn1 getStatus -> [电量, 档位, 充电状态, 外接电源]"""
        p = self.channel.feature(self.dev_index, idx, 1)
        if not p or len(p) < 3:
            return False
        self.percent = _clamp_pct(p[0])
        self.plugged = p[2] in (1, 2, 3)
        self.charging = p[2] in (1, 2)
        self.approx = False
        self.source = '0x1004'
        return True

    def _from_status(self, idx):
        """0x1000 BATTERY_STATUS：fn0 -> [电量, 下一档, 充电状态]"""
        p = self.channel.feature(self.dev_index, idx, 0)
        if not p or len(p) < 3:
            return False
        self.percent = _clamp_pct(p[0])
        self.plugged = p[2] in (1, 2, 3)
        self.charging = p[2] in (1, 2)
        self.approx = False
        self.source = '0x1000'
        return self.percent is not None

    def _from_voltage(self, idx):
        """0x1001 BATTERY_VOLTAGE：fn0 -> [电压高, 电压低, 标志]（只能算个大概）"""
        p = self.channel.feature(self.dev_index, idx, 0)
        if not p or len(p) < 3:
            return False
        mv = p[0] << 8 | p[1]
        self.voltage = mv
        self.percent = voltage_to_percent(mv)
        self.charging = bool(p[2] & 128)
        self.plugged = self.charging
        self.approx = True
        self.source = '0x1001'
        return self.percent is not None

    def _from_registers(self):
        """HID++ 1.0：先试 0x0D（真实百分比），再试 0x07（粗略档位）"""
        p = self.channel.read_register(self.dev_index, REG_BATTERY_CHARGE)
        if p and len(p) >= 3:
            v = p[0]
            if v > 100:
                v = int(round(v * 100.0 / 255.0))
            self.percent = _clamp_pct(v)
            st = p[2] & 240
            self.charging = st in (80, 96)
            self.plugged = st in (80, 96, 144)
            if st == 144 and self.percent == 0:
                self.percent = 100
            self.approx = False
            self.source = 'REG 0x0D'
            return self.percent is not None

        p = self.channel.read_register(self.dev_index, REG_BATTERY_STATUS)
        if p and len(p) >= 2:
            level_code, charging_byte = p[0], p[1]
            self.percent = ROUGH_LEVEL.get(level_code)
            self.approx = True
            if charging_byte == 0:
                self.charging = self.plugged = False
            elif charging_byte & 34 == 34:
                self.charging = False
                self.plugged = True
            elif charging_byte & 33 == 33:
                self.charging = self.plugged = True
            self.source = 'REG 0x07'
            return self.percent is not None
        return False



def _clamp_pct(v):
    if v is None:
        return None
    v = int(v)
    if v < 0:
        return None
    return v if v <= 100 else 100


def _clean_name(raw):
    if isinstance(raw, bytes):
        try:
            text = raw.decode('utf-8', 'ignore')
        except Exception:
            text = raw.decode('latin-1', 'ignore')
    else:
        text = str(raw or '')
    text = ''.join(ch for ch in text if ch.isprintable() or ch in ' ')
    return text.strip()


def receiver_device_name(channel, dev_index):
    """接收器寄存器 0x2B5 里保存的配对设备名（老设备唯一的取名途径）。

子寄存器（对照 Solaar 的 receiver.py）：
  普通 Unifying/Lightspeed : 0x40 + (设备号 - 1)
  Bolt                    : 0x60 + 设备号, 再跟一个 0x01
"""
    if dev_index < 1 or dev_index > 6:
        return ''
    for params in ((64 + dev_index - 1,), (96 + dev_index, 1)):
        try:
            p = channel.read_register(255, REG_RECEIVER_INFO, list(params))
        except Exception:
            p = None
        if not p:
            continue
        raw = bytes(bytearray(p[:16])).split(b'\x00')[0]
        name = _clean_name(raw).strip()


        if name and _NAME_OK.match(name) and any(ch.isalnum() for ch in name):
            return name
    return ''



def open_channels():
    """枚举并打开所有罗技 HID++ 通道（接收器 / 蓝牙 / 有线）"""
    if hid is None:
        return []
    grouped = {}
    for d in hid.enumerate(VID_LOGITECH, 0):
        if d.get('usage_page') != USAGE_PAGE_VENDOR:
            continue
        usage = d.get('usage')
        if usage not in (USAGE_SHORT, USAGE_LONG):
            continue

        tag = _path_tag(d.get('path', ''))
        key = (d.get('product_id'), tag)
        grouped.setdefault(key, {})['short' if usage == USAGE_SHORT else 'long'] = d

    out = []
    for (pid, _tag), dd in grouped.items():
        if 'long' not in dd and 'short' not in dd:
            continue
        try:
            out.append(Channel(pid, _tag,
                              dd.get('short', {}).get('path'),
                              dd.get('long', {}).get('path'),
                              dd.get('short', {}).get('serial_number') or ''))
        except Exception:
            continue
    return out
