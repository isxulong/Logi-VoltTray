
"""
Logi VoltTray · 通用版后台服务

负责：发现设备 -> 识别类型/名字 -> 周期刷新电量 -> 把结果给界面。

设计上沿用旧版验证过的三点：
  * 无线设备深度休眠时读不到，但**保留已识别设备**，后台持续补扫，一唤醒即恢复；
  * 识别结果落盘（known.json），休眠期间也能显示正确的名字/类型；
  * 接收器拔插后自动重开通道。
新增：一个通道上除了 1~6 号配对槽位，还会看 0xFF（蓝牙/USB 线直连设备）。
"""

import json
import os
import re
import tempfile
import threading
import time

from logi_hidpp import KIND_NAME, SCAN_INDEXES, Endpoint, open_channels

APP_FOLDER = 'LogiBatteryPro'



_KIND_BY_NAME = (
    (re.compile(r'^mx\s*keys|^mx\s*mechanical|^craft$|^ergo\s*k|^k\d|tkl|^g9(?:15|13)|^g8(?:15|13)|^g7(?:15|13)',
                 re.I), 'keyboard'),
    (re.compile(r'superlight|\bdex\b|^pro\s*x\s*\d*\s*dex|^mx\s*master|^mx\s*anywhere|^mx\s*vertical|^mx\s*ergo|^m\d|^g\d|trackball',
                 re.I), 'mouse'),
)


def guess_kind_from_name(name):
    if not name:
        return None
    for rx, kind in _KIND_BY_NAME:
        if rx.search(name.strip()):
            return kind
    low = name.lower()
    if 'keyboard' in low or 'kbd' in low:
        return 'keyboard'
    if 'mouse' in low or 'mice' in low:
        return 'mouse'
    return None



def data_dir():
    # 与 logi_config.data_dir 的回退链保持一致：LOCALAPPDATA -> TEMP -> 系统临时目录
    base = os.environ.get('LOCALAPPDATA') or os.environ.get('TEMP') or tempfile.gettempdir()
    d = os.path.join(base, APP_FOLDER)
    try:
        os.makedirs(d, exist_ok=True)
    except Exception:
        pass
    return d


def _cache_path():
    return os.path.join(data_dir(), 'known.json')


def _load_cache():
    data = {}
    try:
        with open(_cache_path(), 'r', encoding='utf-8') as f:
            v = json.load(f)
        if isinstance(v, dict):
            data = v
    except Exception:
        data = {}
    if not data:

        try:
            base = os.environ.get('LOCALAPPDATA') or tempfile.gettempdir()
            with open(os.path.join(base, 'LogiBattery', 'known.json'), 'r',
                      encoding='utf-8') as f:
                old = json.load(f)
            if isinstance(old, dict):
                for k, v in old.items():
                    if isinstance(v, dict) and v.get('kind'):
                        data[k] = {'kind': v['kind'], 'name': v.get('name') or ''}
                _save_cache(data)
        except Exception:
            pass
    return data


def _save_cache(data):
    """原子写：先写临时文件再替换，避免崩溃中途留下半个 JSON 丢掉全部缓存。"""
    path = _cache_path()
    tmp = path + '.tmp'
    try:
        with open(tmp, 'w', encoding='utf-8') as f:
            json.dump(data, f, ensure_ascii=False)
        os.replace(tmp, path)
    except Exception:
        try:
            os.unlink(tmp)
        except Exception:
            pass


def _trace(what, exc=None):
    try:
        with open(os.path.join(data_dir(), 'error.log'), 'a', encoding='utf-8') as f:
            f.write(f"{time.strftime('%Y-%m-%d %H:%M:%S')!s} | {what!s} | {exc!r}\n")
    except Exception:
        pass



class BatteryService(object):
    def __init__(self, interval=2.5, scan_every=2):
        self.interval = interval
        self.scan_every = scan_every
        self.lock = threading.RLock()
        self.devices = []
        self._channels = []
        self._stop = threading.Event()
        self._rescan = False
        self._scanned = False
        self._cache = _load_cache()
        self._thread = None


    @staticmethod
    def _ckey(ep):
        pid, tag, dev = ep.key
        return '%04X|%s|%d' % (pid, tag, dev)

    def _remember(self, ep):
        k = self._ckey(ep)
        want = {'kind': ep.kind, 'name': ep.name}
        if self._cache.get(k) != want and ep.name:
            self._cache[k] = want
            _save_cache(self._cache)

    def _recall(self, channel, dev_index):
        key = '%04X|%s|%d' % (channel.pid, channel.tag, dev_index)
        return self._cache.get(key) or {}


    def start(self):
        self._thread = threading.Thread(target=self._loop, name='logi-battery',
                                        daemon=True)
        self._thread.start()

    def stop(self, wait=True, timeout=10.0):
        """置停止标记；wait=True 时等服务线程真正退出（通道随之关闭）。

        生成诊断报告前必须等线程退出，否则旧通道还开着，报告采集会与之争抢。
        """
        self._stop.set()
        t = self._thread
        if wait and t is not None and t.is_alive() and t is not threading.current_thread():
            t.join(timeout)

    def request_rescan(self):
        self._rescan = True

    @property
    def ready(self):
        return self._scanned

    def snapshot(self):
        with self.lock:
            return [{'name': d.name, 'kind': d.kind, 'percent': d.percent,
                     'charging': d.charging, 'plugged': d.plugged,
                     'approx': d.approx, 'online': d.online,
                     'placeholder': d.placeholder, 'source': d.source,
                     'voltage': d.voltage,
                     'id': '%04X-%d' % (d.channel.pid, d.dev_index)}
                    for d in self._ordered()]

    def _ordered(self):
        rank = {'mouse': 0, 'keyboard': 1, 'device': 2}
        return sorted(self.devices,
                      key=lambda d: (rank.get(d.kind, 2), d.channel.pid, d.dev_index))


    @staticmethod
    def _scan_channel(ch):
        """扫一个通道：1~6 号配对槽位 + 0xFF（蓝牙/线连）。返回识别到的设备列表"""
        found = []
        for dev in SCAN_INDEXES:
            ep = Endpoint(ch, dev)
            try:
                if not ep.probe():
                    continue
                if not ep.identify():
                    continue
            except Exception as exc:
                _trace('识别设备失败 pid=0x%04X dev=%d' % (ch.pid, dev), exc)
                continue
            found.append(ep)
        return found

    def _reopen(self):
        # _scanned（ready）只在首轮扫描真正结束后才置位，
        # 否则托盘会在扫描进行中的几秒内误显示「未找到罗技设备」。
        fresh = []
        try:
            old = {d.key: d for d in self.devices}
            with self.lock:
                for ch in self._channels:
                    try:
                        ch.close()
                    except Exception:
                        pass
                self._channels = []

            try:
                fresh = open_channels()
            except Exception as exc:
                _trace('open_channels 失败', exc)
                fresh = []

            devices = []
            for ch in fresh:
                hit = self._scan_channel(ch)
                for ep in hit:
                    prev = old.get(ep.key)
                    if prev is not None and prev.percent is not None:
                        ep.percent, ep.charging = prev.percent, prev.charging
                        ep.plugged, ep.approx, ep.source = prev.plugged, prev.approx, prev.source
                    if not ep.name:
                        rec = self._recall(ch, ep.dev_index)
                        ep.name = rec.get('name') or ep.name
                        if ep.kind == 'device' and rec.get('kind'):
                            ep.kind = rec['kind']
                    if ep.kind == 'device':
                        ep.kind = guess_kind_from_name(ep.name) or 'device'
                    ep.name = ep.name or KIND_NAME.get(ep.kind, KIND_NAME['device'])
                    self._remember(ep)
                    devices.append(ep)
                if hit: continue

                ph = Endpoint(ch, 1)
                ph.placeholder = True
                rec = self._recall(ch, 1)
                ph.kind = rec.get('kind') or 'device'
                ph.name = rec.get('name') or KIND_NAME.get(ph.kind, KIND_NAME['device'])
                devices.append(ph)

            with self.lock:
                self._channels = fresh
                self.devices = devices
        except Exception as exc:
            _trace('_reopen 异常', exc)
            for ch in fresh:
                try:
                    ch.close()
                except Exception:
                    pass
            with self.lock:
                self._channels = []
                self.devices = []
        finally:
            with self.lock:
                self._scanned = True

    def _refresh_all(self):
        with self.lock:
            devices = list(self.devices)
        alive = 0
        for d in devices:
            if d.placeholder:
                continue
            try:
                if d.refresh():
                    alive += 1
            except Exception:
                d.online = False
        return alive

    def _scan_new(self):
        """补扫：之前休眠、现在刚唤醒的设备"""
        with self.lock:
            known = {d.key for d in self.devices if not d.placeholder}
            channels = list(self._channels)
        found = []
        for ch in channels:
            for dev in SCAN_INDEXES:
                if (ch.pid, ch.tag, dev) in known:
                    continue
                ep = Endpoint(ch, dev)
                try:
                    if not ep.probe() or not ep.identify():
                        continue
                    if not ep.name:
                        rec = self._recall(ch, dev)
                        ep.name = rec.get('name') or ''
                        if ep.kind == 'device' and rec.get('kind'):
                            ep.kind = rec['kind']
                    if ep.kind == 'device':
                        ep.kind = guess_kind_from_name(ep.name) or 'device'
                    ep.name = ep.name or KIND_NAME.get(ep.kind, KIND_NAME['device'])
                    ep.refresh()
                    self._remember(ep)
                    found.append(ep)
                except Exception:
                    continue
        if found:
            with self.lock:
                for ep in found:
                    self.devices = [x for x in self.devices
                                    if not (x.placeholder and x.channel is ep.channel)]
                    if any(x.key == ep.key for x in self.devices): continue
                    self.devices.append(ep)
        return len(found)

    def _rebuild_placeholders(self):
        with self.lock:
            have = {d.channel.tag for d in self.devices}
            channels = list(self._channels)
            for ch in channels:
                if ch.tag not in have:
                    ph = Endpoint(ch, 1)
                    ph.placeholder = True
                    # 与 _reopen 的占位逻辑一致：尽量用缓存里的名字/类型
                    rec = self._recall(ch, 1)
                    ph.kind = rec.get('kind') or 'device'
                    ph.name = rec.get('name') or KIND_NAME.get(ph.kind, KIND_NAME['device'])
                    self.devices.append(ph)


    def _loop(self):
        self._reopen()
        tick = 0
        miss = 0
        while not self._stop.is_set():
            if self._rescan:
                self._rescan = False
                self._reopen()
                tick = 0

            alive = self._refresh_all()
            miss = 0 if alive else miss + 1

            tick += 1
            if tick % self.scan_every == 0:
                self._scan_new()
                self._rebuild_placeholders()

            if miss >= 6:
                miss = 0
                self._reopen()

            if self._stop.wait(self.interval):
                break

        with self.lock:
            for ch in self._channels:
                try:
                    ch.close()
                except Exception:
                    pass
            self._channels = []



if __name__ == '__main__':
    svc = BatteryService()
    svc.start()
    time.sleep(1.0)
    for _ in range(5):
        print('----')
        for d in svc.snapshot():
            pct = '--' if d['percent'] is None else str(d['percent'])
            print(
                f"{d['name'][:10]!s:10} {KIND_NAME.get(d['kind'], d['kind'])!s:6} {pct!s:>4}%  充电="
                f"{d['charging']!s:5} 在线={d['online']!s:5} 来源={d['source'] or '-'!s:9} 占位={d['placeholder']!s}")
        time.sleep(2.0)
    svc.stop()
