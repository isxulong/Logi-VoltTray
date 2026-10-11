
"""
Logi VoltTray · 设备诊断报告

把本机能看到的罗技 HID 通道、每个设备槽位的应答、可用的电量特征全部打出来。
用途：网友反馈"我的键鼠用不了"时，让他把这个报告发回来，就能定位卡在哪一步。

用法：
    LogiVoltTray.exe --report            # 生成报告文件并提示路径
    python logi_report.py               # 命令行直接看
"""

import os
import sys
import time

try:
    import hid
except Exception:
    hid = None

import logi_hidpp as H


def _fmt(b):
    if b is None:
        return "(无应答)"
    if isinstance(b, H._Err):
        return "错误 0x%02X" % (b.code or 0)
    return " ".join("%02X" % x for x in b) if b else "(空)"


def collect():
    out = []
    w = out.append

    w("Logi VoltTray · 通用版 设备诊断报告")
    w("时间: %s" % time.strftime("%Y-%m-%d %H:%M:%S"))
    w("Python: %s   hidapi: %s" % (sys.version.split()[0],
                                 getattr(hid, '__version__', '?') if hid else '不可用'))
    w("")

    w("======================================================================")
    w("一、系统枚举到的罗技 HID 接口")
    w("======================================================================")
    n = 0
    if hid is None:
        w("(hidapi 模块不可用，无法枚举 HID 接口)")
    else:
        for d in hid.enumerate(0, 0):
            if d.get("vendor_id") != H.VID_LOGITECH:
                continue
            path = d.get("path")
            if isinstance(path, bytes):
                path = path.decode("utf-8", "replace")
            w("PID=0x%04X  usage_page=0x%04X usage=0x%04X  iface=%s" % (
                d.get("product_id") or 0, d.get("usage_page") or 0,
                d.get("usage") or 0, d.get("interface_number")))
            w("      %s" % path)
            n += 1
        if not n:
            w("(没有枚举到任何罗技设备)")
    w("")

    channels = H.open_channels() if hid is not None else []
    w("======================================================================")
    w("二、HID++ 通道 %d 个" % len(channels))
    w("======================================================================")
    for ch in channels:
        w("")
        w("通道 PID=0x%04X  tag=%s  长报文口=%s" % (ch.pid, ch.tag, ch.has_long))
        for dev in H.SCAN_INDEXES:
            try:
                hidpp2 = ch.feature_index(dev, H.FEAT_FEATURE_SET, 0.3)
            except Exception as exc:
                hidpp2 = f"异常 {exc!r}"
            reg0d = ch.read_register(dev, H.REG_BATTERY_CHARGE, (), 0.3)
            reg07 = ch.read_register(dev, H.REG_BATTERY_STATUS, (), 0.3)
            if not hidpp2 and reg0d is None and reg07 is None:
                continue
            w("  [设备索引 %d]" % dev)
            w("     HID++2 FEATURE_SET(0x0001) 索引 : %s" % hidpp2)
            w("     HID++1 寄存器 0x0D 电量        : %s" % _fmt(reg0d))
            w("     HID++1 寄存器 0x07 状态        : %s" % _fmt(reg07))

            for fid, label in ((H.FEAT_UNIFIED_BATTERY, "0x1004 UNIFIED_BATTERY"),
                               (H.FEAT_BATTERY_STATUS, "0x1000 BATTERY_STATUS"),
                               (H.FEAT_BATTERY_VOLTAGE, "0x1001 BATTERY_VOLTAGE"),
                               (H.FEAT_DEVICE_NAME, "0x0005 DEVICE_NAME")):
                idx = ch.feature_index(dev, fid, 0.3)
                if not idx:
                    w("     %-24s: 不支持" % label)
                    continue
                if fid == H.FEAT_DEVICE_NAME:
                    cnt = ch.feature(dev, idx, 0)
                    name = ch.feature(dev, idx, 1, [0])
                    w("     %-24s: idx=0x%02X 长度=%s 首片=%s" % (
                        label, idx, cnt[0] if cnt else "?",
                        bytes(bytearray(name)).decode("utf-8", "replace") if name else "?"))
                else:
                    fn = 1 if fid == H.FEAT_UNIFIED_BATTERY else 0
                    r = ch.feature(dev, idx, fn)
                    w("     %-24s: idx=0x%02X fn%d = %s" % (label, idx, fn, _fmt(r)))
            dn_idx = ch.feature_index(dev, H.FEAT_DEVICE_NAME, 0.3)
            if dn_idx:
                kind = ch.feature(dev, dn_idx, 2)
                if kind:
                    w("     设备类型码(0x0005 fn2)         : 0x%02X -> %s" % (
                        kind[0], H.DEVICE_KIND_CODE.get(kind[0], "?")))
            nm = H.receiver_device_name(ch, dev)
            if not nm: continue
            w("     接收器寄存器 0x2B5 设备名       : %s" % nm)

        nm = H.receiver_device_name(ch, 1)
        if nm:
            w("  (接收器寄存器 0x2B5 设备1 名称: %s)" % nm)
        try:
            ch.close()
        except Exception:
            pass

    w("")
    w("======================================================================")
    w("三、最终识别结果")
    w("======================================================================")
    from logi_core import BatteryService, KIND_NAME
    svc = BatteryService()
    svc.start()
    for _ in range(8):
        time.sleep(1.0)
        snap = svc.snapshot()
        if any(d["percent"] is not None for d in snap):
            break
    for d in svc.snapshot():
        w(
            f"{d['name'][:22]:22} {KIND_NAME.get(d['kind'], d['kind']):4} "
            f"{'--' if d['percent'] is None else d['percent']:>4}%  充电="
            f"{d['charging']:5} 约={d['approx']:5} 在线={d['online']:5} 来源={d['source'] or '-':9} id={d['id']}")
    svc.stop(timeout=5.0)
    return "\n".join(out)


def main():
    text = collect()
    if "--report" in sys.argv or "-r" in sys.argv:
        from logi_core import data_dir
        path = os.path.join(data_dir(), "device_report.txt")
        with open(path, "w", encoding="utf-8") as f:
            f.write(text)
        print(path)
        try:
            os.startfile(os.path.dirname(path))
        except Exception:
            pass
    else:
        print(text)


if __name__ == "__main__":
    main()
