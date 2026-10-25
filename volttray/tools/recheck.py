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


# ---------------------------------------------------------------- CSV

def csv_field(s):
    if any(c in s for c in (",", '"', "\n", "\r")):
        return '"%s"' % s.replace('"', '""')
    return s


CSV_HEADER = ("device_key,display_name,segment_type,start_ts,end_ts,"
              "duration_min,start_pct,end_pct,delta_pct,rate_pct_per_hour,note")


def render_csv(segments, anon, off):
    rows = sorted(((display_key(s["device_key"], anon), s) for s in segments),
                  key=lambda x: (x[0], x[1]["start_ts"]))
    out = ["﻿" + CSV_HEADER]
    for key, s in rows:
        r = rate(s)
        out.append(",".join([
            csv_field(key), csv_field(s["display_name"]), s["seg_type"],
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


def check_csv(ck, path, segments, keys, off):
    with open(path, "r", encoding="utf-8") as f:
        body = f.read()
    first_col = set()
    for line in body.splitlines()[1:]:
        if line:
            first_col.add(line.split(",", 1)[0].strip('"'))
    anon = anonymize_map(keys) if detect_anon(first_col, keys) else None
    want = render_csv(segments, anon, off)
    if body != want:
        gl, wl = body.splitlines(), want.splitlines()
        ck.eq("csv 行数", len(gl), len(wl))
        for i, (g, w) in enumerate(zip(gl, wl)):
            ck.eq("csv 第 %d 行" % (i + 1), g, w)
    else:
        print("PASS csv：%d 行逐字节一致（匿名化=%s）" %
              (len(body.splitlines()) - 1, "开" if anon else "关"))


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
    want_events = []
    for ev in events:
        if ev[0] == "read":
            _, ts, pk, dn, pct, chg, src = ev
            want_events.append({"ts": ts, "physical_key": display_key(pk, anon),
                                "display_name": dn, "percent": pct,
                                "charging": chg, "source": src})
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
            want_events.append({"ts": ev[1], "event": "activity_buckets",
                                "physical_key": display_key(ev[2], anon),
                                "days": ev[3]})
    ge = got.get("events", [])
    if ge != want_events:
        ck.eq("json.events 条数", len(ge), len(want_events))
        for i, (g, w) in enumerate(zip(ge, want_events)):
            ck.eq("json.events[%d]" % i, g, w)
    else:
        print("PASS json：%d 条事件逐值一致（匿名化=%s）" %
              (len(ge), "开" if anon else "关"))


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
    # 设备清单
    name_of = {}
    for ev in events:
        if ev[0] == "read":
            name_of[display_key(ev[2], anon)] = ev[3]
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
        "device_key": s["device_key"], "display_name": s["display_name"],
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
    # 融合稿 Phase 0 新结构存在性核验（逐值复算以 chart1/3、segments、CSV 为界）
    for key in ("daily_cards", "run_spans_by_day", "first_screen_kpis",
                "avg_estimate", "week_over_week", "activity_gap_note"):
        if key not in got:
            ck.eq("html.%s 存在" % key, "缺失", "存在")
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
    args = ap.parse_args()
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
        check_csv(ck, args.csv, segments, keys, args.tz)
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
