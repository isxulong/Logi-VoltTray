#!/usr/bin/env python3
"""生成协议黄金样例 JSON（fixtures/golden/*.json）。

样例 schema（tests/golden.rs 消费）：
- kind=response:  {name, kind, hex, expect:{type:"frame", report, device_index, feature_index, function, sw_id, params_hex}}
                  或 expect:{type:"error", device_index, feature_index, error_code}
- kind=malformed: {name, kind, hex, expect:{kind:"EmptyFrame"|"BadReportId"|"BadLength", ...}}
- kind=request:   {name, kind, command, device_index, sw_id, features:{<feature_id 十进制字符串>: index}, expect:{hex}}
hex 为空格分隔字节（可含换行）。
"""
import json
import os

OUT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "golden")
os.makedirs(OUT, exist_ok=True)

REPORT_SHORT = 0x10
REPORT_LONG = 0x11


def hx(bs):
    return " ".join(f"{b:02X}" for b in bs)


def resp_frame(name, long_frame, dev, feat, func, sw, params):
    report = REPORT_LONG if long_frame else REPORT_SHORT
    plen = 16 if long_frame else 3
    assert len(params) == plen
    data = {
        "name": name,
        "kind": "response",
        "hex": hx([report, dev, feat, (func << 4) | sw] + list(params)),
        "expect": {
            "type": "frame",
            "report": report,
            "device_index": dev,
            "feature_index": feat,
            "function": func,
            "sw_id": sw,
            "params_hex": hx(params),
        },
    }
    write(name, data)


def resp_error(name, dev, orig_feat, sw, code, hidpp1=False):
    feat = 0x8F if hidpp1 else 0xFF
    params = [orig_feat, code, 0]
    data = {
        "name": name,
        "kind": "response",
        "hex": hx([REPORT_SHORT, dev, feat, sw, *params]),
        "expect": {
            "type": "error",
            "device_index": dev,
            "feature_index": orig_feat,
            "error_code": code,
        },
    }
    write(name, data)


def malformed(name, hexs, expect):
    write(name, {"name": name, "kind": "malformed", "hex": hexs, "expect": expect})


def request(name, command, dev, sw, features, expect_hex):
    write(name, {
        "name": name,
        "kind": "request",
        "command": command,
        "device_index": dev,
        "sw_id": sw,
        "features": {str(k): v for k, v in features.items()},
        "expect": {"hex": expect_hex},
    })


def write(name, data):
    path = os.path.join(OUT, f"{name}.json")
    with open(path, "w", encoding="utf-8") as f:
        json.dump(data, f, indent=2, ensure_ascii=False)
        f.write("\n")


# ---------- 正常响应 ----------
resp_frame("unified_battery_80_charging", True, 0x01, 0x06, 0, 7,
           [0x50, 0x00, 0x01] + [0] * 13)  # 80%, charging
resp_frame("unified_battery_45_discharging", True, 0x01, 0x06, 0, 8,
           [0x2D, 0x00, 0x00] + [0] * 13)  # 45%
resp_frame("battery_status_90", False, 0x02, 0x04, 0, 5,
           [0x5A, 0x80, 0x00])  # 90%, charging bit
resp_frame("battery_voltage_3900mv", False, 0x02, 0x05, 0, 6,
           [0x0F, 0x3C, 0x00])  # 3900 mV
resp_frame("hidpp1_reg0d_70_charging", False, 0x03, 0x0D, 0, 2,
           [0x46, 0x40, 0x00])  # 70%, status bit
resp_frame("protocol_version_2_0", False, 0x01, 0x00, 1, 1,
           [0x02, 0x00, 0x00])
resp_frame("feature_index_resp_unified", False, 0x01, 0x01, 0, 4,
           [0x06, 0x00, 0x00])
resp_frame("long_frame_generic_fn2", True, 0xFF, 0x09, 2, 15,
           list(range(16)))

# ---------- 设备错误帧（9 个规范错误码 + 未知码 + 1.0 错误帧） ----------
ERRORS = [
    (0x01, "err_01_unknown"), (0x02, "err_02_invalid_argument"),
    (0x03, "err_03_out_of_sequence"), (0x04, "err_04_invalid_param_value"),
    (0x05, "err_05_wrong_device"), (0x06, "err_06_invalid_feature_index"),
    (0x07, "err_07_invalid_function_id"), (0x08, "err_08_busy"),
    (0x09, "err_09_unsupported"),
]
for code, name in ERRORS:
    resp_error(name, 0x01, 0x06, 7, code)
resp_error("err_unknown_code_7f", 0x01, 0x06, 7, 0x7F)
resp_error("err_hidpp1_8f_invalid_arg", 0x02, 0x0D, 3, 0x02, hidpp1=True)

# ---------- 畸形帧 ----------
malformed("malformed_bad_report_id", hx([0x12, 0x01, 0x06, 0x07, 0, 0, 0]),
          {"kind": "BadReportId", "report": 0x12})
malformed("malformed_short_bad_length", hx([0x10, 0x01, 0x06, 0x07, 0, 0]),
          {"kind": "BadLength", "report": 0x10, "expected": 7, "actual": 6})
malformed("malformed_empty", "", {"kind": "EmptyFrame"})

# ---------- 请求编码 ----------
FT = {1: 1, 4096: 4, 4097: 5, 4100: 6}
request("req_battery_unified", "GetBatteryUnified", 0x01, 7, FT, hx([0x10, 0x01, 0x06, 0x07, 0, 0, 0]))
request("req_battery_voltage", "GetBatteryVoltage", 0x02, 3, FT, hx([0x10, 0x02, 0x05, 0x03, 0, 0, 0]))
request("req_get_feature_index_unified", "GetFeatureIndex", 0x01, 2, FT, hx([0x10, 0x01, 0x00, 0x02, 0x10, 0x04, 0]))
request("req_protocol_version", "GetProtocolVersion", 0x01, 1, FT, hx([0x10, 0x01, 0x00, 0x11, 0, 0, 0]))
request("req_hidpp1_reg0d", "ReadHidpp1Register0x0D", 0x03, 5, FT, hx([0x10, 0x03, 0x0D, 0x05, 0, 0, 0]))

print(f"generated {len(os.listdir(OUT))} golden samples in {OUT}")
