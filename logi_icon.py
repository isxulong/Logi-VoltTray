
"""
Logi VoltTray · 图标绘制

画一个系统原生风格的电池图标：圆角外壳 + 按电量填充 + 内嵌数字。
  * 4 倍超采样渲染，边缘平滑（抗锯齿）
  * 描边／数字颜色跟随系统主题（深色任务栏用浅色，反之用深色）
  * 填充色区分设备：鼠标=蓝，键盘=绿，未知=灰；低电量自动转橙／红
"""

import ctypes
from ctypes import wintypes

import logi_config as cfg

user32 = ctypes.WinDLL("user32", use_last_error=True)
gdi32 = ctypes.WinDLL("gdi32", use_last_error=True)

SS = 4
FW_BOLD = 700
FW_NORMAL = 400
ANTIALIASED_QUALITY = 4
DEFAULT_CHARSET = 1
OUT_TT_PRECIS = 5
CLIP_DEFAULT_PRECIS = 0
DEFAULT_PITCH = 0
FF_DONTCARE = 0
TRANSPARENT = 1
NULL_BRUSH = 5
NULL_PEN = 8
DT_CENTER = 1
DT_VCENTER = 4
DT_SINGLELINE = 32
DT_NOPREFIX = 2048


MOUSE_TINT = (47, 125, 225)
KEYBOARD_TINT = (18, 161, 80)
WARN_TINT = (232, 137, 12)
LOW_TINT = (224, 52, 43)
UNKNOWN_TINT = (124, 135, 148)

INK_ON_DARK = (240, 240, 240)
INK_ON_LIGHT = (24, 24, 24)


class BMIH(ctypes.Structure):
    _fields_ = [("biSize", ctypes.c_uint32), ("biWidth", ctypes.c_int32),
                ("biHeight", ctypes.c_int32), ("biPlanes", ctypes.c_uint16),
                ("biBitCount", ctypes.c_uint16), ("biCompression", ctypes.c_uint32),
                ("biSizeImage", ctypes.c_uint32), ("biXPelsPerMeter", ctypes.c_int32),
                ("biYPelsPerMeter", ctypes.c_int32), ("biClrUsed", ctypes.c_uint32),
                ("biClrImportant", ctypes.c_uint32)]


class BMI(ctypes.Structure):
    _fields_ = [("bmiHeader", BMIH), ("bmiColors", ctypes.c_uint32 * 3)]


class SZ(ctypes.Structure):
    _fields_ = [("cx", ctypes.c_long), ("cy", ctypes.c_long)]


class ICONINFO(ctypes.Structure):
    _fields_ = [("fIcon", wintypes.BOOL), ("xHotspot", wintypes.DWORD),
                ("yHotspot", wintypes.DWORD), ("hbmMask", wintypes.HBITMAP),
                ("hbmColor", wintypes.HBITMAP)]


user32.GetDC.restype = wintypes.HDC
user32.GetDC.argtypes = [wintypes.HWND]
user32.ReleaseDC.argtypes = [wintypes.HWND, wintypes.HDC]
user32.FillRect.argtypes = [wintypes.HDC, ctypes.POINTER(wintypes.RECT), wintypes.HBRUSH]
user32.DrawTextW.argtypes = [wintypes.HDC, wintypes.LPCWSTR, ctypes.c_int,
                            ctypes.POINTER(wintypes.RECT), wintypes.UINT]
user32.CreateIconIndirect.restype = wintypes.HICON
user32.CreateIconIndirect.argtypes = [ctypes.POINTER(ICONINFO)]
user32.DestroyIcon.argtypes = [wintypes.HICON]

gdi32.CreateCompatibleDC.restype = wintypes.HDC
gdi32.CreateCompatibleDC.argtypes = [wintypes.HDC]
gdi32.DeleteDC.argtypes = [wintypes.HDC]
gdi32.CreateDIBSection.restype = wintypes.HBITMAP
gdi32.CreateDIBSection.argtypes = [wintypes.HDC, ctypes.POINTER(BMI), wintypes.UINT,
                                  ctypes.POINTER(ctypes.c_void_p), wintypes.HANDLE,
                                  wintypes.DWORD]
gdi32.CreateBitmap.restype = wintypes.HBITMAP
gdi32.CreateBitmap.argtypes = [ctypes.c_int, ctypes.c_int, wintypes.UINT,
                              wintypes.UINT, ctypes.c_void_p]
gdi32.SelectObject.restype = wintypes.HANDLE
gdi32.SelectObject.argtypes = [wintypes.HDC, wintypes.HANDLE]
gdi32.DeleteObject.argtypes = [wintypes.HANDLE]
gdi32.CreateSolidBrush.restype = wintypes.HBRUSH
gdi32.CreateSolidBrush.argtypes = [wintypes.COLORREF]
gdi32.CreatePen.restype = wintypes.HPEN
gdi32.CreatePen.argtypes = [ctypes.c_int, ctypes.c_int, wintypes.COLORREF]
gdi32.RoundRect.argtypes = [wintypes.HDC, ctypes.c_int, ctypes.c_int, ctypes.c_int,
                            ctypes.c_int, ctypes.c_int, ctypes.c_int]
gdi32.CreateRoundRectRgn.restype = wintypes.HRGN
gdi32.CreateRoundRectRgn.argtypes = [ctypes.c_int] * 6
gdi32.SelectClipRgn.argtypes = [wintypes.HDC, wintypes.HRGN]
gdi32.CreateFontW.restype = wintypes.HANDLE
gdi32.CreateFontW.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_int, ctypes.c_int,
                              ctypes.c_int, wintypes.DWORD, wintypes.DWORD, wintypes.DWORD,
                              wintypes.DWORD, wintypes.DWORD, wintypes.DWORD,
                              wintypes.DWORD, wintypes.DWORD, wintypes.LPCWSTR]
gdi32.SetBkMode.argtypes = [wintypes.HDC, ctypes.c_int]
gdi32.SetTextColor.argtypes = [wintypes.HDC, wintypes.COLORREF]
gdi32.GetStockObject.restype = wintypes.HANDLE
gdi32.GetStockObject.argtypes = [ctypes.c_int]
gdi32.GetTextExtentPoint32W.argtypes = [wintypes.HDC, wintypes.LPCWSTR, ctypes.c_int,
                                       ctypes.POINTER(SZ)]

_font_cache = {}


def _c(rgb):
    return rgb[0] | rgb[1] << 8 | rgb[2] << 16


def theme_ink():
    return INK_ON_LIGHT if cfg.theme_is_light() else INK_ON_DARK


def tint_for(kind, level):
    """按设备类型与电量选填充色"""
    if level is not None:
        if level <= 15:
            return LOW_TINT
        if level <= 40:
            return WARN_TINT
    return {'mouse': MOUSE_TINT, 'keyboard': KEYBOARD_TINT}.get(kind, UNKNOWN_TINT)


def _mkfont(h, name, weight, width=0):
    key = (int(h), name, weight, int(width))
    f = _font_cache.get(key)
    if f is None:
        if len(_font_cache) >= 96:
            # 缓存封顶：HFONT 是 GDI 句柄，无上限缓存会在长期运行中耗尽 GDI 资源
            for old in _font_cache.values():
                try:
                    if old:
                        gdi32.DeleteObject(old)
                except Exception:
                    pass
            _font_cache.clear()
        f = gdi32.CreateFontW(-int(h), int(width), 0, 0, weight, 0, 0, 0, DEFAULT_CHARSET,
            OUT_TT_PRECIS, CLIP_DEFAULT_PRECIS, ANTIALIASED_QUALITY,
            DEFAULT_PITCH | FF_DONTCARE, name or "Segoe UI")
        _font_cache[key] = f
    return f


def _text_width(mem, text, name, h, width=0, weight=None):
    f = _mkfont(h, name, weight or FW_BOLD, width)
    old = gdi32.SelectObject(mem, f)
    sz = SZ()
    gdi32.GetTextExtentPoint32W(mem, text, len(text), ctypes.byref(sz))
    gdi32.SelectObject(mem, old)
    return sz.cx



MIN_SQUEEZE = 0.72


def _pick_font(mem, text, name, h, avail_w):
    """挑一个字号 h 的字体；宽度不够时按需横向压缩（lfWidth），而不是缩小字号"""
    nat = _text_width(mem, text, name, h)
    if nat <= 0 or nat <= avail_w:
        return _mkfont(h, name, FW_BOLD, 0)
    width = max(3, int(round(avail_w / max(1, len(text)))))
    f = _mkfont(h, name, FW_BOLD, width)
    old = gdi32.SelectObject(mem, f)
    sz = SZ()
    gdi32.GetTextExtentPoint32W(mem, text, len(text), ctypes.byref(sz))
    gdi32.SelectObject(mem, old)
    if sz.cx <= avail_w:
        return f

    while width > 3:
        width -= 1
        f = _mkfont(h, name, FW_BOLD, width)
        old = gdi32.SelectObject(mem, f)
        sz = SZ()
        gdi32.GetTextExtentPoint32W(mem, text, len(text), ctypes.byref(sz))
        gdi32.SelectObject(mem, old)
        if sz.cx <= avail_w:
            return f
    return f



def _geom(size):
    """电池外壳几何（尽量撑满图标框）"""
    stroke = max(1.0, size * 0.055)
    nub_w = max(1.3, size * 0.065)
    bx0 = stroke * 0.5
    bx1 = size - nub_w - stroke * 0.5
    by0 = size * 0.09
    by1 = size * 0.91
    rad = (by1 - by0) * 0.21
    return {
        "stroke": stroke, "nub_w": nub_w, "bx0": bx0, "bx1": bx1,
        "by0": by0, "by1": by1, "rad": rad,
        "ix0": bx0 + stroke, "iy0": by0 + stroke,
        "ix1": bx1 - stroke, "iy1": by1 - stroke}



def text_box(size, style="num"):
    """数字可用的空间（最终像素）：返回 (x0, y0, x1, y1)"""
    if style == "num":
        pad = size * 0.025
        return pad, pad, size - pad, size - pad
    g = _geom(size)
    return g["ix0"], g["iy0"], g["ix1"], g["iy1"]


def fit_font_height(size, text, font_name="", cap=None, style="num"):
    """在不溢出的前提下，数字最多能用多大字号（宽度不够时允许横向收窄）"""
    x0, y0, x1, y1 = text_box(size, style)
    avail_w = x1 - x0
    avail_h = y1 - y0
    hi = int(round(avail_h * 0.97))
    if cap:
        hi = min(hi, int(cap))
    text = text or "100"
    hdc = user32.GetDC(None)
    mem = gdi32.CreateCompatibleDC(hdc)
    h = hi
    while h >= 5:
        nat = _text_width(mem, text, font_name, h)
        if nat <= 0:
            break
        if nat <= avail_w or avail_w / float(nat) >= MIN_SQUEEZE:
            break
        h -= 1
    gdi32.DeleteDC(mem)
    user32.ReleaseDC(None, hdc)
    return max(4, h)



def _draw(mem, U, size, text, level, tint, ink, font_name, font_size, style):
    k = U / float(size)

    def R(v):
        return int(round(v * k))

    if style == "num":
        _draw_digits(mem, U, size, text, tint, font_name, font_size, k, R)
        return

    g = _geom(size)
    stroke, nub_w = g["stroke"], g["nub_w"]
    bx0, bx1, by0, by1 = g["bx0"], g["bx1"], g["by0"], g["by1"]
    rad = g["rad"]
    ix0, iy0, ix1, iy1 = g["ix0"], g["iy0"], g["ix1"], g["iy1"]


    if level is not None and level > 0:
        rgn = gdi32.CreateRoundRectRgn(R(ix0), R(iy0), R(ix1) + 1, R(iy1) + 1,
            max(1, R(rad)), max(1, R(rad)))
        gdi32.SelectClipRgn(mem, rgn)
        fw = ix0 + (ix1 - ix0) * min(100.0, level) / 100.0
        b = gdi32.CreateSolidBrush(_c(tint))
        rc = wintypes.RECT(R(ix0), R(iy0), R(fw) + 1, R(iy1) + 1)
        user32.FillRect(mem, ctypes.byref(rc), b)
        gdi32.DeleteObject(b)
        gdi32.SelectClipRgn(mem, None)
        gdi32.DeleteObject(rgn)


    p = gdi32.CreatePen(0, max(1, R(stroke)), _c(ink))
    oldp = gdi32.SelectObject(mem, p)
    oldb = gdi32.SelectObject(mem, gdi32.GetStockObject(NULL_BRUSH))
    gdi32.RoundRect(mem, R(bx0), R(by0), R(bx1), R(by1), R(rad * 2), R(rad * 2))
    gdi32.SelectObject(mem, oldb)
    gdi32.SelectObject(mem, oldp)
    gdi32.DeleteObject(p)


    ny0 = by0 + (by1 - by0) * 0.3
    ny1 = by1 - (by1 - by0) * 0.3
    b = gdi32.CreateSolidBrush(_c(ink))
    oldb = gdi32.SelectObject(mem, b)
    gdi32.SelectObject(mem, gdi32.GetStockObject(NULL_PEN))
    gdi32.RoundRect(mem, R(bx1), R(ny0), R(bx1 + nub_w), R(ny1),
        max(1, R(stroke)), max(1, R(stroke)))
    gdi32.SelectObject(mem, oldb)
    gdi32.DeleteObject(b)

    if not text:
        return


    avail_w = ix1 - ix0 - size * 0.01
    avail_h = iy1 - iy0
    h = int(round(font_size)) if font_size else int(round(avail_h * 0.95))
    h = max(4, min(h, int(round(avail_h * 1.05))))
    f = _pick_font(mem, text, font_name, h * k, avail_w * k)

    oldf = gdi32.SelectObject(mem, f)
    gdi32.SetBkMode(mem, TRANSPARENT)
    gdi32.SetTextColor(mem, _c(ink))
    rc = wintypes.RECT(R(bx0), R(by0), R(bx1), R(by1))
    user32.DrawTextW(mem, text, -1, ctypes.byref(rc),
        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX)
    gdi32.SelectObject(mem, oldf)


def _pass(U, fn, bg):
    """在纯 bg 背景上画一遍，返回 BGRA 字节"""
    hdc = user32.GetDC(None)
    mem = gdi32.CreateCompatibleDC(hdc)
    bmi = BMI()
    bmi.bmiHeader.biSize = ctypes.sizeof(BMIH)
    bmi.bmiHeader.biWidth = U
    bmi.bmiHeader.biHeight = -U
    bmi.bmiHeader.biPlanes = 1
    bmi.bmiHeader.biBitCount = 32
    bmi.bmiHeader.biCompression = 0
    ppv = ctypes.c_void_p()
    bmp = gdi32.CreateDIBSection(hdc, ctypes.byref(bmi), 0, ctypes.byref(ppv), None, 0)
    if not bmp or not ppv:
        # GDI 资源耗尽等极端情况下不能往空指针里读，直接失败让上层保留旧图标
        gdi32.DeleteDC(mem)
        user32.ReleaseDC(None, hdc)
        raise RuntimeError("CreateDIBSection failed")
    gdi32.SelectObject(mem, bmp)
    br = gdi32.CreateSolidBrush(_c(bg))
    rc = wintypes.RECT(0, 0, U, U)
    user32.FillRect(mem, ctypes.byref(rc), br)
    gdi32.DeleteObject(br)
    fn(mem, U)
    data = ctypes.string_at(ppv, U * U * 4)
    gdi32.DeleteObject(bmp)
    gdi32.DeleteDC(mem)
    user32.ReleaseDC(None, hdc)
    return data


def _draw_digits(mem, U, size, text, tint, font_name, font_size, k, R):
    """纯数字样式：没有外壳，数字撑满整个图标"""
    if not text:
        return
    x0, y0, x1, y1 = text_box(size, "num")
    avail_w = x1 - x0
    avail_h = y1 - y0
    h = int(round(font_size)) if font_size else int(round(avail_h * 0.97))
    h = max(4, min(h, int(round(avail_h * 1.05))))
    f = _pick_font(mem, text, font_name, h * k, avail_w * k)
    oldf = gdi32.SelectObject(mem, f)
    gdi32.SetBkMode(mem, TRANSPARENT)
    gdi32.SetTextColor(mem, _c(tint))
    rc = wintypes.RECT(R(x0), R(y0), R(x1), R(y1))
    user32.DrawTextW(mem, text, -1, ctypes.byref(rc),
        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX)
    gdi32.SelectObject(mem, oldf)


def render_bgra(size, text, level=None, kind="", font="", font_size=0, ink=None, tint=None,
                style="num"):
    """渲染成 size×size 的 BGRA（直通 alpha），边缘已抗锯齿"""
    ink = ink or theme_ink()
    tint = tint or tint_for(kind, level)
    if style == "num" and cfg.theme_is_light():
        tint = tuple(max(0, int(v * 0.72)) for v in tint)
    U = size * SS

    def fn(mem, u):
        _draw(mem, u, size, text, level, tint, ink, font, font_size, style)

    black = _pass(U, fn, (0, 0, 0))
    white = _pass(U, fn, (255, 255, 255))

    out = bytearray(size * size * 4)
    N = SS * SS
    for oy in range(size):
        for ox in range(size):
            sb0 = sb1 = sb2 = 0.0
            sa = 0.0
            for dy in range(SS):
                base = ((oy * SS + dy) * U + ox * SS) * 4
                for dx in range(SS):
                    i = base + dx * 4
                    a = (255 - (white[i] - black[i])
                         + (255 - (white[i + 1] - black[i + 1]))
                         + (255 - (white[i + 2] - black[i + 2]))) / 765.0
                    if a <= 0.0:
                        continue
                    sa += a
                    sb0 += black[i]
                    sb1 += black[i + 1]
                    sb2 += black[i + 2]
            j = (oy * size + ox) * 4
            if sa <= 0.0:
                continue
            out[j] = max(0, min(255, int(round(sb0 / sa))))
            out[j + 1] = max(0, min(255, int(round(sb1 / sa))))
            out[j + 2] = max(0, min(255, int(round(sb2 / sa))))
            out[j + 3] = max(0, min(255, int(round(sa / N * 255))))
    return bytes(out)


def make_icon(size, text, level=None, kind="", font="", font_size=0, ink=None,
              style="num"):
    """生成托盘图标 HICON（失败返回 None，调用方保留旧图标）"""
    size = max(8, int(size))
    try:
        bgra = render_bgra(size, text, level, kind, font, font_size, ink=ink, style=style)
    except Exception:
        return None


    pm = bytearray(bgra)
    for i in range(0, len(pm), 4):
        a = pm[i + 3]
        if a == 255:
            continue
        if a == 0:
            pm[i] = pm[i + 1] = pm[i + 2] = 0
            continue
        pm[i] = pm[i] * a // 255
        pm[i + 1] = pm[i + 1] * a // 255
        pm[i + 2] = pm[i + 2] * a // 255

    hdc = user32.GetDC(None)
    bmi = BMI()
    bmi.bmiHeader.biSize = ctypes.sizeof(BMIH)
    bmi.bmiHeader.biWidth = size
    bmi.bmiHeader.biHeight = -size
    bmi.bmiHeader.biPlanes = 1
    bmi.bmiHeader.biBitCount = 32
    bmi.bmiHeader.biCompression = 0
    ppv = ctypes.c_void_p()
    hbmp = gdi32.CreateDIBSection(hdc, ctypes.byref(bmi), 0, ctypes.byref(ppv), None, 0)
    if not hbmp:
        user32.ReleaseDC(None, hdc)
        return
    ctypes.memmove(ppv, bytes(pm), len(pm))

    stride = (size + 15) // 16 * 2
    mask = (ctypes.c_ubyte * (stride * size))()
    hmask = gdi32.CreateBitmap(size, size, 1, 1, mask)

    ii = ICONINFO()
    ii.fIcon = True
    ii.hbmMask = hmask
    ii.hbmColor = hbmp
    hicon = user32.CreateIconIndirect(ctypes.byref(ii))

    gdi32.DeleteObject(hbmp)
    if hmask:
        gdi32.DeleteObject(hmask)
    user32.ReleaseDC(None, hdc)
    return hicon


if __name__ == "__main__":
    import shot as S

    cases = [(20, '92', 'mouse'), (20, '100', 'keyboard'), (20, '8', 'mouse'), (20, '--', ''), (16, '92', 'mouse'), (16, '100', 'keyboard')]

    MAG = 9
    cw, ch = 20 * MAG + 24, 20 * MAG + 24
    W, H = cw * len(cases) + 20, ch + 20
    out = bytearray(b'\x00' * (W * H * 4))
    for i in range(W * H):
        out[i * 4:i * 4 + 4] = bytes((31, 31, 31, 255))
    for ci, (sz, txt, kind) in enumerate(cases):
        lv = None if txt == "--" else int(txt)
        px = render_bgra(sz, txt, lv, kind)
        ox, oy = 20 + ci * cw + 8, 20
        for y in range(sz):
            for x in range(sz):
                i = (y * sz + x) * 4
                a = px[i + 3] / 255.0
                for dy in range(MAG):
                    for dx in range(MAG):
                        j = ((oy + y * MAG + dy) * W + ox + x * MAG + dx) * 4
                        for c in range(3):
                            out[j + c] = int(px[i + c] * a + out[j + c] * (1 - a))
    S.write_png("_icon_new.png", W, H, bytes(out))
    print("saved _icon_new.png", W, H)
