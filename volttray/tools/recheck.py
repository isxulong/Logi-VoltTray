#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""WP-B（I-08）复算脚本：LogiVoltTray 耗电统计导出的独立对账基准。

纯 Python 标准库镜像 crates/app/src/attribution.rs 的分段归因与渲染
口径（AC-B2/B8 的验收基准）：
  jsonl（skip-bad-lines）→ 锚点流（run 标记归并、同 ts 排在设备事件后）
  → 元素段（判定表）→ coalesce → ≥1h 单轮不级联合并 → 排序
  → CSV 行 / volttray-data（chart1/3、分日日卡等融合稿 Phase 0 结构）
  / JSON 事件序列（含 activity_buckets 分钟位图摘要）。

用法：
  python3 tools/recheck.py --stats stats.jsonl [--archive stats.archive.jsonl] \
      [--csv 导出.csv] [--html 导出.html] [--json 导出.json] --tz 480

--tz 为导出时的本地时区偏移（分钟，+08:00 为 480）。CSV/HTML/JSON 至少
给一个；匿名化状态自动检测（device_key 形如 D1/D2… 即按匿名化复算）。
退出码：全部一致 0；任一不一致 1（首个差异点打印到 stderr）。

时钟回拨（Δt≤0）负时出口径（质检 F1 钉死，本脚本与 attribution.rs
逐值一致的唯一基准）：
  1. 回拨段保留实际 ts 区间（end<start）作为异常证据，不钳位、不重排；
  2. duration_min 对负时长按数学向下取整（floor，`(dur+30)//60`，
     ≡ Rust `(secs+30).div_euclid(60)`）；
  3. coalesce 仅合并「两段均非倒置且时序邻接」的同型段——倒置段不
     参与合并、独立保留，coalesce 绝不产出 end_ts < start_ts 的段。
"""
import argparse
import json
import math
import re
import sys

MERGE_WINDOW = 3600
GAP_OVER_7D = 7 * 86400
BUCKET = 3600

NOTE_RUN_GAP = "run_gap_unattributed"
NOTE_EVIDENCE_MISSING = "evidence_missing"
NOTE_BOUNDARY = "boundary"
NOTE_BELOW_WINDOW = "below_window"
NOTE_CLOCK_ANOMALY = "clock_anomaly"
NOTE_GAP_OVER_7D = "gap_over_7d"

SOURCES = {"poll", "charging-fast", "wake-first-read"}
WFR = "wake-first-read"


# ---------------------------------------------------------------- 基础工具

def rust_round(x):
    """Rust f64::round：half away from zero。"""
    return math.floor(x + 0.5) if x >= 0 else math.ceil(x - 0.5)


def round2(x):
    return rust_round(x * 100.0) / 100.0


def round4(x):
    return rust_round(x * 10000.0) / 10000.0


def civil_from_days(z):
    z = z + 719468
    era = (z if z >= 0 else z - 146096) // 146097
    doe = z - era * 146097
    yoe = (doe - doe // 1460 + doe // 36524 - doe // 146096) // 365
    y = yoe + era * 400
    doy = doe - (365 * yoe + yoe // 4 - yoe // 100)
    mp = (5 * doy + 2) // 153
    d = doy - (153 * mp + 2) // 5 + 1
    m = mp + 3 if mp < 10 else mp - 9
    return (y + 1 if m <= 2 else y), m, d


def fmt_rfc3339(ts, off):
    local = ts + off * 60
    days = local // 86400
    secs = local % 86400
    y, m, d = civil_from_days(days)
    hh, mm, ss = secs // 3600, (secs % 3600) // 60, secs % 60
    sign = "-" if off < 0 else "+"
    o = abs(off)
    return "%04d-%02d-%02dT%02d:%02d:%02d%s%02d:%02d" % (
        y, m, d, hh, mm, ss, sign, o // 60, o % 60)


def fmt_local_date(ts, off):
    y, m, d = civil_from_days((ts + off * 60) // 86400)
    return "%04d-%02d-%02d" % (y, m, d)


def local_day_start(ts, off):
    return ((ts + off * 60) // 86400) * 86400 - off * 60


def fnv1a64_hex(s):
    h = 14695981039346656037
    for b in s.encode("utf-8"):
        h ^= b
        h = (h * 1099511628211) % (1 << 64)
    return "%016x" % h


# ---------------------------------------------------------------- 事件解析

def parse_line(line):
    line = line.strip()
    if not line:
        return None
    try:
        o = json.loads(line)
    except ValueError:
        return None
    if not isinstance(o, dict):
        return None
    ts = o.get("ts")
    if not isinstance(ts, int) or isinstance(ts, bool):
        return None
    ev = o.get("event")
    if ev is None:
        pk, dn = o.get("physical_key"), o.get("display_name")
        pct, chg, src = o.get("percent"), o.get("charging"), o.get("source")
        if not (isinstance(pk, str) and isinstance(dn, str)
                and isinstance(pct, int) and not isinstance(pct, bool)
                and 0 <= pct <= 100 and isinstance(chg, bool)
                and src in SOURCES):
            return None
        return ("read", ts, pk, dn, pct, chg, src)
    if ev == "run_start":
        return ("run_start", ts)
    if ev == "run_stop":
        return ("run_stop", ts)
    if ev == "boundary":
        devs = o.get("devices")
        if not isinstance(devs, list):
            return None
        out = []
        for d in devs:
            if not isinstance(d, dict):
                return None
            pk, pct, chg = d.get("physical_key"), d.get("percent"), d.get("charging")
            if not (isinstance(pk, str) and isinstance(pct, int)
                    and not isinstance(pct, bool) and 0 <= pct <= 100
                    and isinstance(chg, bool)):
                return None
            out.append((pk, pct, chg))
        return ("boundary", ts, out)
    if ev == "sleep_enter":
        pk = o.get("physical_key")
        if not isinstance(pk, str):
            return None
        return ("sleep_enter", ts, pk)
    if ev == "activity_buckets":
        pk = o.get("physical_key")
        days = o.get("days")
        if not isinstance(pk, str) or not isinstance(days, list):
            return None
        for d in days:
            if not (isinstance(d, dict)
                    and isinstance(d.get("date"), str)
                    and isinstance(d.get("bitmap_hex"), str)):
                return None
        return ("activity_buckets", ts, pk, days)
    return None


def load_events(paths):
    out = []
    for p in paths:
        try:
            with open(p, "r", encoding="utf-8") as f:
                body = f.read()
        except OSError:
            continue
        for line in body.splitlines():
            ev = parse_line(line)
            if ev is not None:
                out.append(ev)
    return out


# ---------------------------------------------------------------- 分段引擎
# 锚点: (ts, kind, pct|None, charging|None, wake_first_read)
# kind: read / sleep_enter / run_start / run_stop / boundary

def build_segments(events):
    per_device = {}
    names = {}
    run_marks = []
    for ev in events:
        k = ev[0]
        if k == "read":
            _, ts, pk, dn, pct, chg, src = ev
            per_device.setdefault(pk, []).append((ts, "read", pct, chg, src == WFR))
            names[pk] = dn
        elif k == "sleep_enter":
            _, ts, pk = ev
            per_device.setdefault(pk, []).append((ts, "sleep_enter", None, None, False))
        elif k == "boundary":
            _, ts, devs = ev
            for pk, pct, chg in devs:
                per_device.setdefault(pk, []).append((ts, "boundary", pct, chg, False))
        elif k == "run_start":
            run_marks.append((ev[1], "run_start"))
        elif k == "run_stop":
            run_marks.append((ev[1], "run_stop"))
    out = []
    for key in sorted(per_device):
        anchors = per_device[key]
        combined = list(anchors)
        for ts, kind in run_marks:
            pos = next((i for i, a in enumerate(combined) if a[0] > ts), len(combined))
            combined.insert(pos, (ts, kind, None, None, False))
        segs = segment_device(key, combined, names.get(key, ""))
        merge_below_window(segs)
        out.extend(segs)
    out.sort(key=lambda s: (s["device_key"], s["start_ts"]))
    return out


def segment_device(key, anchors, display_name):
    out = []
    carried = None  # (pct, charging)
    sleeping = False
    prev = None
    for cur in anchors:
        ts, kind, pct, chg, wfr = cur
        if prev is not None:
            dt = ts - prev[0]
            notes = []
            start_pct = carried[0] if carried else 0
            end_pct = pct if pct is not None else start_pct
            if kind == "run_start":
                seg_type = "unknown"
                notes.append(NOTE_RUN_GAP)
                end_pct = start_pct
            elif dt <= 0:
                seg_type = "unknown"
                notes.append(NOTE_CLOCK_ANOMALY)
            elif dt > GAP_OVER_7D:
                seg_type = "unknown"
                notes.append(NOTE_GAP_OVER_7D)
            elif carried is None:
                seg_type = "unknown"
                notes.append(NOTE_EVIDENCE_MISSING)
            elif sleeping:
                seg_type = "sleep"
            elif kind == "read" and wfr:
                seg_type = "unknown"
                notes.append(NOTE_EVIDENCE_MISSING)
            else:
                seg_type = "charging" if carried[1] else "active"
            if kind == "boundary":
                notes.append(NOTE_BOUNDARY)
            if carried is None:
                start_pct = 0
                end_pct = 0
            out.append({
                "device_key": key, "display_name": display_name,
                "seg_type": seg_type, "start_ts": prev[0], "end_ts": ts,
                "start_pct": start_pct, "end_pct": end_pct,
                "notes": notes, "merged": False,
            })
        if kind == "read" or kind == "boundary":
            carried = (pct, chg)
            if kind == "read":
                sleeping = False
        elif kind == "sleep_enter":
            sleeping = True
        prev = cur
    # coalesce（质检 F1 钉死，与 attribution.rs 逐行镜像同规则）：仅当
    # 两段均非倒置（end>=start）且时序邻接（后段起点不早于前段终点）
    # 才合并——时钟回拨产生的倒置段不参与合并、独立保留为异常证据；
    # 正常段合并后 end 只进不退，coalesce 绝不产出 end_ts < start_ts
    # 的段。
    merged = []
    for seg in out:
        if merged:
            last = merged[-1]
            sequential = (
                last["start_ts"] <= last["end_ts"]
                and seg["start_ts"] <= seg["end_ts"]
                and seg["start_ts"] >= last["end_ts"]
            )
            if last["seg_type"] == seg["seg_type"] and sequential:
                last["end_ts"] = seg["end_ts"]
                last["end_pct"] = seg["end_pct"]
                for n in seg["notes"]:
                    if n not in last["notes"]:
                        last["notes"].append(n)
                continue
        merged.append(seg)
    return merged


def dur(s):
    return s["end_ts"] - s["start_ts"]


def merge_below_window(segs):
    i = 0
    while i < len(segs):
        s = segs[i]
        if dur(s) >= MERGE_WINDOW or s["merged"]:
            i += 1
            continue
        t = s["seg_type"]
        prev_ok = i > 0 and not segs[i - 1]["merged"] and segs[i - 1]["seg_type"] == t
        next_ok = i + 1 < len(segs) and not segs[i + 1]["merged"] and segs[i + 1]["seg_type"] == t
        if prev_ok and next_ok:
            target_prev = dur(segs[i - 1]) >= dur(segs[i + 1])
        elif prev_ok:
            target_prev = True
        elif next_ok:
            target_prev = False
        else:
            s["notes"].append(NOTE_BELOW_WINDOW)
            i += 1
            continue
        winner, loser = (i - 1, i) if target_prev else (i + 1, i)
        loser_seg = segs.pop(loser)
        if winner > loser:
            winner -= 1
        w = segs[winner]
        if loser_seg["start_ts"] < w["start_ts"]:
            w["start_ts"] = loser_seg["start_ts"]
            w["start_pct"] = loser_seg["start_pct"]
        if loser_seg["end_ts"] > w["end_ts"]:
            w["end_ts"] = loser_seg["end_ts"]
            w["end_pct"] = loser_seg["end_pct"]
        for n in loser_seg["notes"]:
            if n not in w["notes"]:
                w["notes"].append(n)
        w["merged"] = True
        if dur(w) < MERGE_WINDOW and NOTE_BELOW_WINDOW not in w["notes"]:
            w["notes"].append(NOTE_BELOW_WINDOW)
        if not target_prev:
            i += 1
    for s in segs:
        s["merged"] = False


# ---------------------------------------------------------------- 派生量

def duration_min(s):
    # duration_min 列：四舍五入到整数分钟（(secs+30)/60）；负时长（时钟
    # 回拨段）按数学向下取整 floor——Python // 即 floor，与 Rust 侧
    # `(secs+30).div_euclid(60)` 同口径（质检 F1 钉死；非负值与截断除法
    # 等价，如 -100s → -2 而非 -1）。
    return (dur(s) + 30) // 60


def delta_pct(s):
    return s["end_pct"] - s["start_pct"]


MIN_RATE_DURATION_SECS = 300
MIN_RATE_ABS_DELTA_PCT = 1


def rate(s):
    # 速率护栏（attribution.rs rate_pct_per_hour 镜像）：unknown / 非正
    # 时长 / <5 分钟短段 / |Δ%|<1 一律 None（CSV 留空 / JSON null）。
    if (s["seg_type"] == "unknown" or dur(s) <= 0
            or dur(s) < MIN_RATE_DURATION_SECS
            or abs(delta_pct(s)) < MIN_RATE_ABS_DELTA_PCT):
        return None
    return round2(delta_pct(s) / (dur(s) / 3600.0))


def note_text(s):
    return ";".join(s["notes"])


def event_keys(events):
    keys = set()
    for ev in events:
        if ev[0] == "read":
            keys.add(ev[2])
        elif ev[0] in ("sleep_enter", "activity_buckets"):
            keys.add(ev[2])
        elif ev[0] == "boundary":
            for pk, _, _ in ev[2]:
                keys.add(pk)
    return keys


def anonymize_map(keys):
    hashed = sorted(((fnv1a64_hex(k), k) for k in keys), key=lambda x: (x[0], x[1]))
    return {k: "D%d" % (i + 1) for i, (_, k) in enumerate(hashed)}


def display_key(key, anon):
    return anon.get(key, key) if anon else key


def display_name_session_map(events):
    """组5 5b（C-3）：会话内 display_name 首现顺序 → 设备N
    （attribution::display_name_session_order + redact::device_session_map
    镜像；空名跳过；编号不承担匿名化职责）。"""
    order = []
    for ev in events:
        if ev[0] == "read":
            dn = ev[3]
            if dn and dn not in order:
                order.append(dn)
    return {dn: "设备%d" % (i + 1) for i, dn in enumerate(order)}


def map_display_name(dn, names):
    return names.get(dn, dn) if names else dn


# ---------------------------------------------------------------- CSV

def csv_field(s):
    if any(c in s for c in (",", '"', "\n", "\r")):
        return '"%s"' % s.replace('"', '""')
    return s


CSV_HEADER = ("device_key,display_name,segment_type,start_ts,end_ts,"
              "duration_min,start_pct,end_pct,delta_pct,rate_pct_per_hour,note")


def render_csv(segments, anon, names, off):
    rows = sorted(((display_key(s["device_key"], anon), s) for s in segments),
                  key=lambda x: (x[0], x[1]["start_ts"]))
    out = ["﻿" + CSV_HEADER]
    for key, s in rows:
        r = rate(s)
        out.append(",".join([
            csv_field(key), csv_field(map_display_name(s["display_name"], names)),
            s["seg_type"],
            fmt_rfc3339(s["start_ts"], off), fmt_rfc3339(s["end_ts"], off),
            str(duration_min(s)), str(s["start_pct"]), str(s["end_pct"]),
            str(delta_pct(s)), "" if r is None else "%.2f" % r,
            csv_field(note_text(s)),
        ]))
    return "\n".join(out) + "\n"


# ---------------------------------------------------------------- 图表数据

def build_sticky_spans(segments):
    """粘滞段清单（attribution.rs sticky_spans 镜像，原始键口径）：
    Active/Sleep 放电段、Δ%=0、时长 >24h。"""
    out = {}
    for s in segments:
        if (s["seg_type"] in ("active", "sleep")
                and s["start_pct"] == s["end_pct"]
                and dur(s) > 86400):
            out.setdefault(s["device_key"], []).append(
                [s["start_ts"], s["end_ts"]])
    return out


def build_chart1(segs, devices, sticky):
    if not segs:
        return []
    min_ts = min(s["start_ts"] for s in segs)
    max_ts = max(s["end_ts"] for s in segs)
    fb, lb = min_ts // BUCKET, max_ts // BUCKET
    out = []
    for key, name in devices:
        ds = [s for s in segs if s["device_key"] == key]
        points = []
        for b in range(fb, lb + 1):
            b0, b1 = b * BUCKET, b * BUCKET + BUCKET
            covering = [s for s in ds if s["start_ts"] < b1 and s["end_ts"] > b0]
            last = covering[-1] if covering else None
            v = last["end_pct"] if (last and last["seg_type"] != "unknown") else None
            points.append([b0, v])
        spans = lambda t: [[s["start_ts"], s["end_ts"]] for s in ds if s["seg_type"] == t]
        out.append({"device_key": key, "display_name": name, "points": points,
                    "unknown_spans": spans("unknown"), "charging_spans": spans("charging"),
                    "sticky_spans": sticky.get(key, [])})
    return out



def build_chart3(segs):
    per_dev, total = {}, {"active": 0, "sleep": 0, "charging": 0, "unknown": 0}
    for s in segs:
        d = max(0, dur(s))
        cell = per_dev.setdefault(s["device_key"],
                                  {"active": 0, "sleep": 0, "charging": 0, "unknown": 0})
        cell[s["seg_type"]] += d
        total[s["seg_type"]] += d
    return dict(sorted(per_dev.items())), total



# ---------------------------------------------------------------- 对账

class Checker:
    def __init__(self):
        self.failures = 0

    def eq(self, label, got, want):
        if got != want:
            self.failures += 1
            print("FAIL %s\n  got : %r\n  want: %r" % (label, got, want), file=sys.stderr)
            return False
        return True


def detect_anon(keys_present, raw_keys):
    """CSV/JSON/HTML 中出现 D1/D2… 形式键 → 匿名化导出。"""
    if not keys_present:
        return False
    if all(re.fullmatch(r"D\d+", k) for k in keys_present):
        return True
    return False


def check_csv(ck, path, segments, keys, off, events):
    with open(path, "r", encoding="utf-8") as f:
        body = f.read()
    # 组5 自描述导出（N-12②）：CSV 头部新增 `#` manifest 注释块（BOM
    # 与表头之间）。对账口径：剥 BOM → 丢弃 `#` 注释行 → 重加 BOM，再
    # 与 render_csv（不含注释块）逐字节比对。注释块内容本身不在此断言
    # ——schema_version 等字段以 JSON/HTML manifest 落点的对账为准；
    # 此处只要求注释行存在且形如 `# ...`（不影响表头与数据行的字节级
    # 一致性校验）。
    body_cmp = body
    if body_cmp.startswith("\ufeff"):
        body_cmp = body_cmp[1:]
    kept = [l for l in body_cmp.split("\n") if not l.startswith("#")]
    body_cmp = "\ufeff" + "\n".join(kept)
    first_col = set()
    for line in body_cmp.splitlines()[1:]:
        if line:
            first_col.add(line.split(",", 1)[0].strip('"'))
    anon = anonymize_map(keys) if detect_anon(first_col, keys) else None
    # 组5 5b：镜像默认闸门（redact_display_name=开）——匿名化在场时
    # display_name 按会话映射「设备N」。
    names = display_name_session_map(events) if anon else None
    want = render_csv(segments, anon, names, off)
    if body_cmp != want:
        gl, wl = body_cmp.splitlines(), want.splitlines()
        ck.eq("csv 行数", len(gl), len(wl))
        for i, (g, w) in enumerate(zip(gl, wl)):
            ck.eq("csv 第 %d 行" % (i + 1), g, w)
    else:
        n_comments = len(body.splitlines()) - len(body_cmp.splitlines())
        print("PASS csv：%d 行逐字节一致（匿名化=%s，跳过 manifest 注释行 %d 行）" %
              (len(body_cmp.splitlines()) - 1, "开" if anon else "关", n_comments))
        # 复核二 P2-3：原始字节比对（不剥 `#` 注释行的旧版工具口径）失败、
        # 且剥后逐字节一致 → 差异仅在 manifest `#` 注释头。给出明确提示，
        # 指向注释头与新版工具，避免旧工具用户误判为数据不一致。
        if n_comments > 0 and body != want:
            print("提示：本 CSV 在 BOM 与表头之间含 %d 行 manifest `#` 注释头；"
                  "不剥注释行的旧版复算工具做字节比对会失败——该失败的差异仅在"
                  " `#` 注释头，数据区已逐字节一致。请使用随 v0.3.26 起发布的新版"
                  " tools/recheck.py（剥 BOM → 丢 `#` 注释行 → 再字节比对）；"
                  "注释头各键口径见注释行 manifest.audit / manifest.diag_audit "
                  "指向的审计文档。" % n_comments)


def check_json(ck, path, events, keys):
    with open(path, "r", encoding="utf-8") as f:
        got = json.load(f)
    ck.eq("json.version", got.get("version"), 1)
    if not isinstance(got.get("exported_at"), int):
        ck.eq("json.exported_at 类型", type(got.get("exported_at")).__name__, "int")
    gk = set()
    for e in got.get("events", []):
        if "physical_key" in e:
            gk.add(e["physical_key"])
        for d in e.get("devices", []):
            gk.add(d.get("physical_key"))
    anon = anonymize_map(keys) if detect_anon(gk, keys) else None
    names = display_name_session_map(events) if anon else None
    want_events = []
    want_buckets = []
    for ev in events:
        if ev[0] == "read":
            _, ts, pk, dn, pct, chg, src = ev
            want_events.append({"ts": ts, "physical_key": display_key(pk, anon),
                                "display_name": map_display_name(dn, names),
                                "percent": pct, "charging": chg, "source": src})
        elif ev[0] == "run_start":
            want_events.append({"ts": ev[1], "event": "run_start"})
        elif ev[0] == "run_stop":
            want_events.append({"ts": ev[1], "event": "run_stop"})
        elif ev[0] == "boundary":
            want_events.append({"ts": ev[1], "event": "boundary", "devices": [
                {"physical_key": display_key(pk, anon), "percent": pct, "charging": chg}
                for pk, pct, chg in ev[2]]})
        elif ev[0] == "sleep_enter":
            want_events.append({"ts": ev[1], "physical_key": display_key(ev[2], anon),
                                "event": "sleep_enter"})
        elif ev[0] == "activity_buckets":
            want_buckets.append({"ts": ev[1], "event": "activity_buckets",
                                 "physical_key": display_key(ev[2], anon),
                                 "days": ev[3]})
    ge = got.get("events", [])
    # 组4 N-08② 位图开关镜像：activity_buckets 为可选段——导出在
    # 位（旧样本 / 勾选位图）则逐值严格复算（BvA-8 向后兼容）；
    # 缺席不判负（新默认 export_activity_bitmap=关，位图不进导出）。
    ge_buckets = [e for e in ge if e.get("event") == "activity_buckets"]
    ge_main = [e for e in ge if e.get("event") != "activity_buckets"]
    if ge_main != want_events:
        ck.eq("json.events 条数", len(ge_main), len(want_events))
        for i, (g, w) in enumerate(zip(ge_main, want_events)):
            ck.eq("json.events[%d]" % i, g, w)
    else:
        print("PASS json：%d 条事件逐值一致（匿名化=%s）" %
              (len(ge_main), "开" if anon else "关"))
    if ge_buckets:
        if ge_buckets != want_buckets:
            ck.eq("json.activity_buckets 条数", len(ge_buckets), len(want_buckets))
            for i, (g, w) in enumerate(zip(ge_buckets, want_buckets)):
                ck.eq("json.activity_buckets[%d]" % i, g, w)
        else:
            print("PASS json.activity_buckets：%d 条位图事件逐值一致" %
                  len(ge_buckets))

# ---------------------------------------------------------------- 分日 / KPI 复算
# v0.3.28（P2）：此前 daily_cards / run_spans_by_day / first_screen_kpis /
# avg_estimate / week_over_week / activity_gap_note 只查 key 存在性——前端
# 四个数字没有独立基准。以下逐值镜像 attribution.rs 同名函数，含 v0.3.28
# 的四处口径修复：
#   S1 日耗电按充电段切段后各子段首末差求和（充电回补后重置基准）；
#   S2 冲突徽标与放电覆盖解耦（覆盖 <12h 只挂「覆盖不足」）；
#   S3 单侧口径缺失挂「无法对账」而不是静默当不冲突；
#   S4 源分布进 sparse 判据（当日全是唤醒首读 → 无效样本）。

SPARSE_MIN_READS = 3
SPARSE_MIN_COVERAGE_SECS = 7200
AVG_MIN_VALID_DAYS = 7
AVG_CONFLICT_RATIO = 2.0
AVG_CONFLICT_ABS_THRESHOLD_PCT = 1.0
AVG_CONFLICT_MIN_COVERAGE_HOURS = 12.0
AVG_COVERAGE_SHORT_ESCALATE_RATIO = 10.0
AVG_MIN_WINDOW_SECS = 300
RECENT_DAYS = 7
WOW_GAP_THRESHOLD_DAYS = 2
STICKY_MIN_DURATION_SECS = 86400


def days_from_civil(date):
    """`YYYY-MM-DD` → 日序号（attribution.rs civil_days 镜像）；解析失败 None。"""
    try:
        y, m, d = (int(x) for x in date.split("-"))
    except (ValueError, AttributeError):
        return None
    if not (1 <= m <= 12 and 1 <= d <= 31):
        return None
    y = y - 1 if m <= 2 else y
    era = (y if y >= 0 else y - 399) // 400
    yoe = y - era * 400
    mp = m - 3 if m > 2 else m + 9
    doy = (153 * mp + 2) // 5 + d - 1
    doe = yoe * 365 + yoe // 4 - yoe // 100 + doy
    return era * 146097 + doe - 719468


def median(xs):
    if not xs:
        return None
    xs = sorted(xs)
    n = len(xs)
    if n % 2 == 1:
        return float(xs[n // 2])
    return (xs[n // 2 - 1] + xs[n // 2]) / 2.0


def sticky_day_set(spans, off):
    """粘滞段覆盖的本地日集合（attribution.rs sticky_day_set 镜像）。"""
    out = {}
    for dev, lst in spans.items():
        days = out.setdefault(dev, set())
        for a, b in lst:
            t = a
            while True:
                days.add(fmt_local_date(t, off))
                if t >= b:
                    break
                nxt = local_day_start(t, off) + 86400
                if nxt <= t:
                    break
                t = min(nxt, b)
    return out


def build_daily_cards(events, segments, sticky_days, off):
    """分日视图日卡（attribution.rs build_daily_cards 逐行镜像）。

    日耗电 = 相邻两条读数之间的「下降量」累计（充电态读数同样参与——插电
    期间读数下跌也是真实放电）。归属与分母同规则：以区间起点读数为准，
    下降量记到起点日、跨日间隔按起点日窗口裁剪；放电跨度只累加「起点非
    充电态」或「确有下降」的间隔（纯插电空转不计时长）。reads 仍只收非充
    电读数；charging-fast 一律不收（推断值）。"""
    reads = {}
    series = {}
    for ev in events:
        if ev[0] != "read":
            continue
        _, ts, pk, _dn, pct, chg, src = ev
        if src == "charging-fast":
            continue
        day = fmt_local_date(ts, off)
        series.setdefault(pk, []).append((day, ts, pct, chg))
        if chg:
            continue
        reads.setdefault((pk, day), []).append((ts, pct, src == WFR))
    # 相邻下降量口径：按设备聚合后按时间扫（见函数 docstring）。
    acc = {}
    for dev, ds in series.items():
        ds.sort(key=lambda r: r[1])
        for i in range(len(ds) - 1):
            d0, t0, p0, c0 = ds[i]
            _dd, t1, p1, _c1 = ds[i + 1]
            win_end = local_day_start(t0, off) + 86400
            secs = max(0, min(t1, win_end) - t0)
            drop = max(0, p0 - p1)
            cell = acc.setdefault((dev, d0), [0, 0])
            cell[0] += drop
            if (not c0) or drop > 0:
                cell[1] += secs
    spans = {}
    for seg in segments:
        if seg["seg_type"] not in ("charging", "unknown"):
            continue
        t = seg["start_ts"]
        while t < max(seg["end_ts"], seg["start_ts"] + 1) and t <= seg["end_ts"]:
            d0 = local_day_start(t, off)
            clip = (max(seg["start_ts"], d0), min(seg["end_ts"], d0 + 86400))
            if clip[1] > clip[0]:
                cell = spans.setdefault(
                    (seg["device_key"], fmt_local_date(t, off)), ([], []))
                if seg["seg_type"] == "charging":
                    cell[0].append(clip)
                else:
                    cell[1].append(clip)
            nxt = d0 + 86400
            if nxt <= t:
                break
            t = nxt
    recovery = {}
    for seg in segments:
        if seg["seg_type"] != "charging":
            continue
        gain = max(0, seg["end_pct"] - seg["start_pct"])
        if gain == 0:
            continue
        mid = seg["start_ts"] + dur(seg) // 2
        k = (seg["device_key"], fmt_local_date(mid, off))
        recovery[k] = recovery.get(k, 0) + gain
    out = {}

    for (dev, date) in sorted(reads):
        lst = sorted(reads[(dev, date)], key=lambda r: r[0])
        first_ts, first_pct = lst[0][0], lst[0][1]
        last_ts, last_pct = lst[-1][0], lst[-1][1]
        coverage = last_ts - first_ts
        cspans, uspans = spans.get((dev, date), ([], []))
        acc_cell = acc.get((dev, date), [0, 0])
        drain, discharge_secs = acc_cell[0], acc_cell[1]
        non_wake = sum(1 for (_t, _p, w) in lst if not w)
        sparse = (len(lst) < SPARSE_MIN_READS
                  or coverage < SPARSE_MIN_COVERAGE_SECS
                  or non_wake == 0)
        slope = (round2(drain / (discharge_secs / 3600.0))
                 if (not sparse and discharge_secs >= SPARSE_MIN_COVERAGE_SECS)
                 else None)
        out.setdefault(dev, []).append({
            "date": date,
            "first_ts": first_ts, "first_pct": first_pct,
            "last_ts": last_ts, "last_pct": last_pct,
            "drain_pct": drain, "discharge_secs": discharge_secs,
            "charge_recovery_pct": recovery.get((dev, date), 0),
            "reads": len(lst), "coverage_secs": coverage,
            "slope_pct_per_hour": slope,
            "sticky": date in sticky_days.get(dev, ()),
            "sparse": sparse,
            "charging_spans": [list(x) for x in cspans],
            "unknown_spans": [list(x) for x in uspans],
        })
    for cards in out.values():
        cards.sort(key=lambda c: c["date"])
    return out


def build_run_spans_by_day(events, off):
    """每日 app 运行时段（attribution.rs 镜像：run_start/run_stop 配对、
    崩溃段闭合到上一会话最后事件 ts、日界裁剪）。"""
    spans = []
    open_ts = None
    last_after = None
    for ev in events:
        k = ev[0]
        if k == "run_start":
            if open_ts is not None and last_after is not None and last_after > open_ts:
                spans.append((open_ts, last_after))
            open_ts = ev[1]
            last_after = None
        elif k == "run_stop":
            if open_ts is not None and ev[1] > open_ts:
                spans.append((open_ts, ev[1]))
            open_ts = None
            last_after = None
        else:
            if open_ts is not None:
                last_after = ev[1]
    if open_ts is not None and last_after is not None and last_after > open_ts:
        spans.append((open_ts, last_after))
    out = {}
    for a, b in spans:
        t = a
        while t < b:
            d0 = local_day_start(t, off)
            clip = (max(a, d0), min(b, d0 + 86400))
            if clip[1] > clip[0]:
                out.setdefault(fmt_local_date(t, off), []).append(list(clip))
            nxt = d0 + 86400
            if nxt <= t:
                break
            t = nxt
    return out


def avg_conflict(ledger, upper, ratio=None):
    """双口径冲突判定（attribution.rs avg_conflict / avg_conflict_at 镜像；
    单侧缺失 → False，由 unreconcilable 单独承载）。`ratio` 缺省用常规阈值，
    传 AVG_COVERAGE_SHORT_ESCALATE_RATIO 即「覆盖不足态是否差得离谱」的硬判据。"""
    if ledger is None or upper is None:
        return False
    r = AVG_CONFLICT_RATIO if ratio is None else ratio
    l, u = ledger, upper
    if l < 0.0 or u < 0.0:
        return True
    if u == 0.0:
        return l > AVG_CONFLICT_ABS_THRESHOLD_PCT
    if l <= 0.0:
        return u > 0.0
    return u > r * l or l > r * u


def build_avg_estimate(events, segments, device_key, cards, sticky_days):
    """日均耗电双口径对账（attribution.rs build_avg_estimate 镜像）。"""
    valid_cards = [c for c in cards if not c["sticky"] and not c["sparse"]]
    slopes = [c["slope_pct_per_hour"] for c in valid_cards
              if c["slope_pct_per_hour"] is not None
              and c["slope_pct_per_hour"] >= 0.0]
    valid_days = len(slopes)
    median_slope = median(slopes)
    upper = None if median_slope is None else round2(median_slope * 24.0)
    # 覆盖中位必须与 upper 同一批日卡（Rust 侧 red-d C4 修复同口径）：
    # 只有出得了斜率（非 None 且 >=0）的卡才进覆盖中位。
    coverage_hours = median([c["discharge_secs"] / 3600.0 for c in valid_cards
                             if c["slope_pct_per_hour"] is not None
                             and c["slope_pct_per_hour"] >= 0.0
                             and c["discharge_secs"] / 3600.0 > 0.0])
    if coverage_hours is None:
        coverage_hours = 0.0
    # 只按 ts 稳定排序（与 Rust `sort_by_key(|(ts, _)| *ts)` 同语义）：
    # 同 ts 多读数时保留事件原序，末条才是真正的「最后一个读数」。旧写
    # 法 sorted((ts, pct)) 在同 ts 上会退化为按 pct 升序重排，末条变成
    # 较大 pct，总账 net 少算 1pp（stats_recheck 实测 13.89 vs Rust 14.59）。
    dev_reads = sorted(((ev[1], ev[4]) for ev in events
                        if ev[0] == "read" and ev[2] == device_key
                        and not ev[5] and ev[6] != "charging-fast"),
                       key=lambda x: x[0])
    def recovery_in(t0, t1):
        """窗口内充电回补（段被窗口切开时按时间占比缩放）。"""
        acc = 0.0
        for s in segments:
            if s["device_key"] != device_key or s["seg_type"] != "charging":
                continue
            span = max(1, dur(s))
            lo, hi = max(s["start_ts"], t0), min(s["end_ts"], t1)
            inside = max(0, hi - lo)
            if inside <= 0:
                continue
            gain = max(0, s["end_pct"] - s["start_pct"])
            acc += gain * (inside / float(span))
        return int(rust_round(acc))
    if dev_reads and dev_reads[-1][0] - dev_reads[0][0] >= AVG_MIN_WINDOW_SECS:
        t0, t1 = dev_reads[0][0], dev_reads[-1][0]
        wd = (t1 - t0) / 86400.0
        net = dev_reads[0][1] - dev_reads[-1][1]
        ledger = round2((net + recovery_in(t0, t1)) / wd)
        window_days = round2(wd)
    else:
        ledger, window_days = None, 0.0
    unreconcilable = (ledger is not None) != (upper is not None)
    coverage_short = valid_days > 0 and coverage_hours < AVG_CONFLICT_MIN_COVERAGE_HOURS
    # 覆盖不足不无条件吞掉冲突：超过硬阈值（AVG_COVERAGE_SHORT_ESCALATE_RATIO）
    # 仍报「口径冲突」，两个徽标同时挂（Rust 侧 red-g G4 同口径）。
    hard_gap = (not unreconcilable
                and avg_conflict(ledger, upper, AVG_COVERAGE_SHORT_ESCALATE_RATIO))
    conflict = (not unreconcilable
                and (hard_gap if coverage_short else avg_conflict(ledger, upper)))
    badges = []
    if valid_days < AVG_MIN_VALID_DAYS:
        badges.append("样本不足")
    if sticky_days:
        badges.append("含粘滞")
    if unreconcilable:
        badges.append("无法对账")
    else:
        if coverage_short:
            badges.append("覆盖不足")
        if conflict:
            badges.append("口径冲突")
    return {
        "lower_pct_per_day": ledger,
        "upper_pct_per_day": upper,
        "median_slope_pct_per_hour": median_slope,
        "ledger_pct_per_day": ledger,
        "valid_days": valid_days,
        "window_days": window_days,
        "coverage_hours": coverage_hours,
        "coverage_short": coverage_short,
        "sticky_days": sum(1 for c in cards if c["date"] in sticky_days),
        "conflict": conflict,
        "unreconcilable": unreconcilable,
        "badges": badges,
    }


def build_week_over_week(cards, conflict):
    """周环比（attribution.rs build_week_over_week 镜像）。"""
    desc = list(reversed(cards))
    window = desc[:2 * RECENT_DAYS]
    window_first = window[-1]["date"] if window else None
    window_last = window[0]["date"] if window else None
    badges = []
    asc = list(reversed(window))
    has_gap = False
    for a, b in zip(asc, asc[1:]):
        da, db = days_from_civil(a["date"]), days_from_civil(b["date"])
        if da is not None and db is not None and db - da > WOW_GAP_THRESHOLD_DAYS:
            has_gap = True
    if has_gap:
        badges.append("窗口含缺口")
    recent = desc[:RECENT_DAYS]
    prev = desc[RECENT_DAYS:2 * RECENT_DAYS]
    sticky_hit = any(c["sticky"] for c in recent) or any(c["sticky"] for c in prev)
    if sticky_hit or conflict:
        return {"suppressed": True, "reason": "不可比",
                "delta_pct_per_day": None, "window_first": window_first,
                "window_last": window_last, "badges": badges}
    recent_eff = [c for c in recent if not c["sparse"]]
    prev_eff = [c for c in prev if not c["sparse"]]
    if not prev_eff or not recent_eff:
        return {"suppressed": True, "reason": "样本不足",
                "delta_pct_per_day": None, "window_first": window_first,
                "window_last": window_last, "badges": badges}
    r = sum(c["drain_pct"] for c in recent_eff) / float(len(recent_eff))
    p = sum(c["drain_pct"] for c in prev_eff) / float(len(prev_eff))
    return {"suppressed": False, "reason": None,
            "delta_pct_per_day": round2(r - p), "window_first": window_first,
            "window_last": window_last, "badges": badges}


def check_html(ck, path, events, segments, keys, off):
    with open(path, "r", encoding="utf-8") as f:
        body = f.read()
    m = re.search(r'<script id="volttray-data" type="application/json">(.*?)</script>',
                  body, re.S)
    if not m:
        ck.eq("html volttray-data 数据块", "缺失", "存在")
        return
    got = json.loads(m.group(1))
    # 静态离线核验（AC-B7 口径）：零外链。
    for pat in (r'https?://', r'<link\b', r'<img\b', r'src=',
                r'url\(', r'@import'):
        hits = re.findall(pat, body)
        if pat == r'src=':
            hits = [h for h in re.findall(r'(?:src)\s*=\s*["\']([^"\']*)', body)
                    if h and not h.startswith("#")]
        if pat == r'https?://':
            hits = [h for h in re.findall(r'https?://[^"\'<>\s)]+', body)
                    if h != "http://www.w3.org/2000/svg"]
        ck.eq("html 零外链 %s" % pat, len(hits), 0)
    gk = set(k for k, _ in got.get("devices", []))
    anon = anonymize_map(keys) if detect_anon(gk, keys) else None
    names = display_name_session_map(events) if anon else None
    # 设备清单
    name_of = {}
    for ev in events:
        if ev[0] == "read":
            name_of[display_key(ev[2], anon)] = map_display_name(ev[3], names)
    devices = [[k, v] for k, v in sorted(name_of.items())]
    ck.eq("html.devices", got.get("devices"), devices)
    # 段明细（展示键 + 排序）
    segs = []
    for s in segments:
        c = dict(s)
        c["device_key"] = display_key(c["device_key"], anon)
        segs.append(c)
    segs.sort(key=lambda s: (s["device_key"], s["start_ts"]))
    want_rows = [{
        "device_key": s["device_key"],
        "display_name": map_display_name(s["display_name"], names),
        "segment_type": s["seg_type"], "start_ts": s["start_ts"], "end_ts": s["end_ts"],
        "duration_min": duration_min(s), "start_pct": s["start_pct"],
        "end_pct": s["end_pct"], "delta_pct": delta_pct(s),
        "rate_pct_per_hour": rate(s), "note": note_text(s),
    } for s in segs]
    ck.eq("html.segments", got.get("segments"), want_rows)
    # 图表（chart1 粘滞段 = 原始键检出 → 展示键透出，attribution.rs 镜像）
    sticky_raw = build_sticky_spans(segments)
    sticky = {}
    for raw_k, spans in sticky_raw.items():
        sticky[display_key(raw_k, anon)] = spans
    ck.eq("html.chart1", got.get("chart1"),
          build_chart1(segs, [(k, n) for k, n in devices], sticky))
    pd, total = build_chart3(segs)
    ck.eq("html.chart3", got.get("chart3"), {"per_device": pd, "total": total})
    # 融合稿 Phase 0 新结构逐值复算（复核二 P2）：日卡 / app 运行时段 / 首屏
    # KPI / 双口径对账 / 周环比 / 活动位图缺口卡，全部按 attribution.rs 装配段
    # （build_report_data :1593-1742）逐行镜像——此前这里只查 key 存在性，
    # 前端的今日已耗、日均区间、周环比四个数字没有独立基准。
    # 内部聚合一律在原始键上做（Rust 侧 daily_raw / avg / wow 同口径），
    # 透出前才换展示键；raw_of 由 read 事件反查（与 Rust raw_of 同构造）。
    raw_of = {}
    for ev in events:
        if ev[0] == "read":
            raw_of[display_key(ev[2], anon)] = ev[2]
    sd_days = sticky_day_set(sticky_raw, off)
    daily_raw = build_daily_cards(events, segments, sd_days, off)
    run_by_day = build_run_spans_by_day(events, off)
    # now_ref = 全部事件的最大 ts（含 run/boundary/sleep_enter/activity_buckets），
    # 空事件时回落到 meta.generated_at（Rust 侧回落到 generated_ts 同一值）。
    ts_all = [ev[1] for ev in events]
    now_ref = max(ts_all) if ts_all else got.get("meta", {}).get("generated_at", 0)
    today = fmt_local_date(now_ref, off)
    want_cards, want_avg, want_wow, want_kpis = {}, {}, {}, {}
    for dkey, _nm in devices:
        raw_key = raw_of.get(dkey, dkey)
        cards = daily_raw.get(raw_key, [])
        dev_sd = sd_days.get(raw_key, set())
        avg = build_avg_estimate(events, segments, raw_key, cards, dev_sd)
        wow = build_week_over_week(cards, avg["conflict"])
        latest = None
        for ev in reversed(events):
            if ev[0] == "read" and ev[2] == raw_key:
                latest = (ev[1], ev[4], ev[5])
                break
        today_card = next((c for c in cards if c["date"] == today), None)
        today_run_secs = sum(b - a for a, b in run_by_day.get(today, []))
        if today_card is None:
            today_drain, today_sparse, today_sticky = None, True, False
        else:
            today_drain = None if today_card["sparse"] else today_card["drain_pct"]
            today_sparse = today_card["sparse"]
            today_sticky = today_card["sticky"]
        want_cards[dkey] = cards
        want_avg[dkey] = avg
        want_wow[dkey] = wow
        want_kpis[dkey] = {
            "current_pct": None if latest is None else latest[1],
            "current_ts": None if latest is None else latest[0],
            "current_charging": False if latest is None else latest[2],
            "today_date": today,
            "today_drain_pct": today_drain,
            "today_sparse": today_sparse,
            "today_sticky": today_sticky,
            "today_run_secs": today_run_secs,
            # 首屏 avg 只透 6 个键（不是整个 AvgEstimate）——配对区间 +
            # 单侧缺失标记 + 覆盖跨度 + 徽标，前端据此判灰态。
            "avg": {
                "lower_pct_per_day": avg["lower_pct_per_day"],
                "upper_pct_per_day": avg["upper_pct_per_day"],
                "conflict": avg["conflict"],
                "unreconcilable": avg["unreconcilable"],
                "coverage_hours": avg["coverage_hours"],
                "coverage_short": avg["coverage_short"],
                "badges": avg["badges"],
            },
        }
    ck.eq("html.daily_cards", got.get("daily_cards"), want_cards)
    ck.eq("html.run_spans_by_day", got.get("run_spans_by_day"), run_by_day)
    ck.eq("html.first_screen_kpis", got.get("first_screen_kpis"), want_kpis)
    ck.eq("html.avg_estimate", got.get("avg_estimate"), want_avg)
    ck.eq("html.week_over_week", got.get("week_over_week"), want_wow)
    # 活动位图缺口卡（指标④：无字段不伪造；activity_days = 各事件 days[].date
    # 去重并集，不是单事件的 days.len() 最大值）。
    act_days = set()
    for ev in events:
        if ev[0] == "activity_buckets":
            for d in ev[3]:
                act_days.add(d["date"])
    n_act = len(act_days)
    gap_present = n_act < AVG_MIN_VALID_DAYS
    if n_act == 0:
        gap_text = ("尚无静止/活动字段，不用 app 协议流量伪造。补齐计划："
                    "activity_buckets 分钟位图随系统事件流积累（零设备查询、"
                    "事件驱动），积累 ≥7 天后升级为分日热力带。")
    elif gap_present:
        gap_text = ("静止/活动分钟位图已积累 %d 天（≥7 天升级分日热力带）；"
                    "当前版本仍按缺口口径展示，不用 app 协议流量伪造。" % n_act)
    else:
        gap_text = ("静止/活动分钟位图已积累 %d 天，满足升级条件；分日热力带"
                    "将在后续版本上线，当前仍按缺口口径展示，不用 app 协议"
                    "流量伪造。" % n_act)
    ck.eq("html.activity_gap_note", got.get("activity_gap_note"),
          {"present": gap_present, "text": gap_text})
    ck.eq("html.chart2 已移除", "chart2" in got, False)
    ck.eq("html.recent7d_active_rate 已移除",
          "recent7d_active_rate" in got, False)
    # meta（generated_at/_local 为生成时刻值，只对结构与口径字段）
    gm = got.get("meta", {})
    ck.eq("html.meta.version", gm.get("version"), 2)
    ck.eq("html.meta.anonymized", gm.get("anonymized"), anon is not None)
    ck.eq("html.meta.tz_offset", gm.get("tz_offset"),
          "%s%02d:%02d" % ("-" if off < 0 else "+", abs(off) // 60, abs(off) % 60))
    ck.eq("html.meta.chart2_approx 已移除", "chart2_approx" in gm, False)
    # days_covered = 全量覆盖日计数（covered_days 镜像，不再封顶 7 天）
    all_days = set()
    for sgm in segs:
        t = sgm["start_ts"]
        while t < max(sgm["end_ts"], sgm["start_ts"] + 1) and t <= sgm["end_ts"]:
            all_days.add(fmt_local_date(t, off))
            nxt = local_day_start(t, off) + 86400
            if nxt <= t:
                break
            t = nxt
    ck.eq("html.meta.days_covered", gm.get("days_covered"), len(all_days))
    if ck.failures == 0:
        print("PASS html：volttray-data 逐值一致 + 断网零外链（匿名化=%s）" %
              ("开" if anon else "关"))


# 组3 N-04/05 档位对账期望档（秒）：与 Rust 侧常量同源口径——
# POLL_CHARGING=15（充电 15s 档）、DEFAULT_POLL_SECS=1800（常规默认档）。
TIER_CHARGING_SECS = 15
TIER_NORMAL_SECS = 1800


def tier_audit(events):
    """组3 N-04/05 档位对账（信息性输出，不判负）：按设备 × 档位
    （充电 / 常规，由相邻两条读数的 charging 态共同判定）统计相邻
    读数间隔，给出样本数 / 中位 / 最大 / 跳轮倍数（间隔 ÷ 期望档，
    四舍五入，≥1）。表观读数间隔 = 轮间隔 + 整轮耗时 + 跳轮叠加，
    口径见 decisions.md D-98。"""
    reads = {}
    for ev in events:
        if ev[0] == "read":
            _, ts, pk, _dn, _pct, chg, _src = ev
            reads.setdefault(pk, []).append((ts, chg))
    lines = []
    for pk in sorted(reads):
        xs = sorted(reads[pk])
        buckets = {"charging": [], "normal": []}
        for (t0, c0), (t1, c1) in zip(xs, xs[1:]):
            buckets["charging" if (c0 and c1) else "normal"].append(t1 - t0)
        for tier, expect in (("charging", TIER_CHARGING_SECS),
                             ("normal", TIER_NORMAL_SECS)):
            iv = sorted(buckets[tier])
            if not iv:
                continue
            med = iv[len(iv) // 2]
            mult = max(1, int(med / expect + 0.5))
            lines.append("档位对账 %s [%s]：样本 %d，中位 %ds（≈%d×%ds 档），最大 %ds"
                         % (pk[:32], tier, len(iv), med, mult, expect, iv[-1]))
    return lines


def main():
    # Windows 控制台/管道默认编码（cp1252/GBK）无法输出中文摘要——
    # 统一把 stdout/stderr 重配为 UTF-8（CI windows-latest 实测 cp1252
    # UnicodeEncodeError；Python 3.7+ reconfigure 可用，3.6 及以下跳过）。
    for _stream in (sys.stdout, sys.stderr):
        if hasattr(_stream, "reconfigure"):
            _stream.reconfigure(encoding="utf-8", errors="replace")
    ap = argparse.ArgumentParser(description="WP-B（I-08）耗电统计导出复算对账")
    ap.add_argument("--stats", required=True, help="当前 stats.jsonl")
    ap.add_argument("--archive", help="stats.archive.jsonl（轮转后旧文件，先读）")
    ap.add_argument("--csv", help="导出的 CSV 文件")
    ap.add_argument("--html", help="导出的 HTML 报告")
    ap.add_argument("--json", help="导出的 JSON 事件序列")
    ap.add_argument("--tz", type=int, default=480, help="本地时区偏移（分钟，默认 480=+08:00）")
    ap.add_argument("--tier-audit", action="store_true",
                    help="组3 N-04/05：只做档位对账（读数间隔 × 期望档），信息性输出不判负")
    args = ap.parse_args()
    if args.tier_audit:
        events = load_events(([args.archive] if args.archive else []) + [args.stats])
        lines = tier_audit(events)
        for line in lines:
            print(line)
        print("档位对账：%d 行（设备 %d 个，口径 D-98）" % (len(lines), len(event_keys(events))))
        sys.exit(0)
    if not (args.csv or args.html or args.json):
        ap.error("--csv/--html/--json 至少给一个")

    paths = ([args.archive] if args.archive else []) + [args.stats]
    events = load_events(paths)
    print("事件：%d 条（%s）" % (len(events), " + ".join(paths)))
    segments = build_segments(events)
    print("分段：%d 段（设备 %d 个）" % (len(segments), len(event_keys(events))))
    keys = event_keys(events)
    ck = Checker()
    if args.csv:
        check_csv(ck, args.csv, segments, keys, args.tz, events)
    if args.json:
        check_json(ck, args.json, events, keys)
    if args.html:
        check_html(ck, args.html, events, segments, keys, args.tz)
    if ck.failures:
        print("复算结果：不一致（%d 处）" % ck.failures, file=sys.stderr)
        sys.exit(1)
    print("复算结果：全部一致（AC-B2/B8 口径）")


if __name__ == "__main__":
    main()
