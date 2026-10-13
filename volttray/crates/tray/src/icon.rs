//! 托盘图标渲染与缓存（v0.3.0 设置体系 M9/M11/M12）。
//!
//! - 启动时按当前 DPI 档（100%/150%/200% → 16/24/32px）用 GDI 预渲染
//!   全部文本位图（"0".."99"、"满"、"?"、"--"）× 4 色 × 常态/提亮两档，
//!   运行时只做像素缓存换图标，热路径零 GDI 调用。
//! - 两位数字封顶：100% 显示 "满"；未知 "?"；灰显 "--"。
//! - 颜色档（M12）：≤ 用户阈值 红、≤ 50 黄（固定）、其余默认；灰显灰。
//! - 形状（M9）：rounded 圆底（默认）/ square 直角 / transparent 透明无背景。
//! - 背景色（M9）：light/dark 预设或 #RRGGBB 自定义；透明语义由形状承担。
//! - 字体（M9）：系统字体名（默认 Segoe UI），GDI 缺字体时静默回退。
//! - 闪烁（M11）：低电触发时在常态与「背景提亮 40%」两档间 1Hz 切换；
//!   透明底改为提亮字色。

use std::collections::HashMap;
use volttray_app::config::{
    brighten, contrast_ratio, IconBackground, IconFontColor, IconShape, RgbColor,
};
use windows::core::PCWSTR;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, SelectObject,
    SetBkMode, SetTextColor, TextOutW, ANTIALIASED_QUALITY, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DEFAULT_CHARSET, DIB_RGB_COLORS, HGDIOBJ, RGBQUAD, TRANSPARENT,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE,
    REG_VALUE_TYPE,
};

/// 图标颜色档。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IconColor {
    Default,
    Yellow,
    Red,
    Gray,
    /// 分档色（v0.3.3 #4）：u8 = 档位序号（0 基），颜色由分档颜色表解析。
    Tier(u8),
}

impl IconColor {
    const ALL: [IconColor; 4] = [Self::Default, Self::Yellow, Self::Red, Self::Gray];

    /// 按电量选色（M12：红档阈值 = 用户设定 low_threshold；黄档固定 50）。
    pub fn for_percent(percent: u8, gray: bool, threshold: u8) -> Self {
        if gray {
            Self::Gray
        } else if percent <= threshold {
            Self::Red
        } else if percent <= 50 {
            Self::Yellow
        } else {
            Self::Default
        }
    }
}

/// 白/黑中选对背景对比度更高者（自定义背景 auto 字色用）。
fn high_contrast_bw(bg: (u8, u8, u8)) -> (u8, u8, u8) {
    const WHITE: (u8, u8, u8) = (0xFF, 0xFF, 0xFF);
    const BLACK: (u8, u8, u8) = (0x1B, 0x1B, 0x1B);
    if contrast_ratio(WHITE, bg) >= contrast_ratio(BLACK, bg) {
        WHITE
    } else {
        BLACK
    }
}

/// 解析某电量档的字体 RGB（M9；v0.3.3 #4 增加分档表）。
///
/// - 灰显（未知/离线）固定灰色，不受固定色覆盖；
/// - 分档色（Tier）：直接取分档颜色表；启用分档时单一字色被忽略；
/// - 固定色覆盖正常/警告/低电档；
/// - auto：警告黄、低电红、正常档依背景/形状取高对比
///   （深底白字 / 浅底深字 / 自定义底取黑白中对比更高者 / 透明按系统主题）。
pub fn font_rgb(
    level: IconColor,
    dark_theme: bool,
    shape: IconShape,
    bg: IconBackground,
    font: IconFontColor,
    tiers: Option<&[RgbColor]>,
) -> (u8, u8, u8) {
    match (font, level) {
        // 灰显永远是灰色：未知(?)与离线(--)不参与配色覆盖。
        (_, IconColor::Gray) => (0x80, 0x80, 0x80),
        // 分档色优先于单一字色（#4：启用分档后单一颜色选择被忽略）。
        (_, IconColor::Tier(i)) => tiers
            .and_then(|t| t.get(i as usize))
            .map(|c| (c.r, c.g, c.b))
            .unwrap_or((0x80, 0x80, 0x80)),
        (IconFontColor::Fixed(c), _) => (c.r, c.g, c.b),
        (IconFontColor::Auto, IconColor::Yellow) => (0xFF, 0xD2, 0x1F),
        (IconFontColor::Auto, IconColor::Red) => (0xE8, 0x11, 0x23),
        (IconFontColor::Auto, IconColor::Default) => match shape {
            IconShape::Transparent => {
                if dark_theme {
                    (0xFF, 0xFF, 0xFF)
                } else {
                    (0x1B, 0x1B, 0x1B)
                }
            }
            IconShape::Rounded | IconShape::Square => match bg {
                IconBackground::Dark => (0xFF, 0xFF, 0xFF),
                IconBackground::Light => (0x1B, 0x1B, 0x1B),
                IconBackground::Custom(_) => high_contrast_bw(bg.rgb()),
            },
        },
    }
}

/// 按百分比取图标文本（评审 4.1：两位封顶，100% 显示"满"）。
pub fn icon_text(percent: Option<u8>, gray: bool) -> String {
    if gray {
        return "--".to_string();
    }
    match percent {
        Some(100) => "满".to_string(),
        Some(p) => format!("{}", p.min(99)),
        None => "?".to_string(),
    }
}

pub fn dpi_bucket_size(dpi: u32) -> u32 {
    // v0.3.3 #5：连续尺寸取代 16/24/32 三档——125% 这类中间 DPI 原先被
    // 钳到 16px 再拉伸到 20px 槽位，是高分屏模糊的直接原因；现按 DPI
    // 线性取整（96→16、120→20、144→24、192→32），并钳制在 16..=48。
    ((dpi as f64 * 16.0 / 96.0).round() as u32).clamp(16, 48)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 读系统主题（HKCU\...\Personalize\AppsUseLightTheme）；读不到按深色处理。
pub fn system_dark_theme() -> bool {
    let sub = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
    let name = wide("AppsUseLightTheme");
    unsafe {
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(sub.as_ptr()),
            None,
            KEY_QUERY_VALUE,
            &mut hkey,
        )
        .is_err()
        {
            return true;
        }
        let mut ty = REG_VALUE_TYPE(0);
        let mut data = 1u32;
        let mut size = 4u32;
        let r = RegQueryValueExW(
            hkey,
            PCWSTR::from_raw(name.as_ptr()),
            None,
            Some(&mut ty),
            Some(&mut data as *mut u32 as *mut u8),
            Some(&mut size),
        );
        let _ = RegCloseKey(hkey);
        if r.is_err() {
            return true;
        }
        data == 0
    }
}

/// 闪烁时的背景提亮系数（M11：约 +40%）。
const BLINK_BRIGHTEN: f64 = 0.4;

/// 预渲染位图缓存：key = (颜色档, 文本, 提亮档)。
pub struct IconCache {
    pub size: u32,
    dark_theme: bool,
    shape: IconShape,
    background: IconBackground,
    font: IconFontColor,
    font_family: String,
    /// 分档颜色表（v0.3.3 #4）；None = 未启用分档（走 IconColor::ALL）。
    tier_colors: Option<Vec<RgbColor>>,
    map: HashMap<(IconColor, String, bool), Vec<u8>>,
}

impl IconCache {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        size: u32,
        dark_theme: bool,
        shape: IconShape,
        background: IconBackground,
        font: IconFontColor,
        font_family: String,
        tiers: Option<Vec<RgbColor>>,
    ) -> Self {
        let mut cache = IconCache {
            size,
            dark_theme,
            shape,
            background,
            font,
            font_family,
            tier_colors: tiers,
            map: HashMap::new(),
        };
        cache.fill();
        cache
    }

    fn fill(&mut self) {
        self.map.clear();
        let mut texts: Vec<String> = (0..=99).map(|i| i.to_string()).collect();
        texts.push("满".to_string());
        texts.push("?".to_string());
        texts.push("--".to_string());
        let shape = self.shape;
        let bg_rgb = self.background.rgb();
        let face = self.font_family.clone();
        let tiers = self.tier_colors.as_deref();
        // 颜色档集合：分档模式 = N 个分档色 + 灰；否则 = 默认 4 档。
        let colors: Vec<IconColor> = match tiers {
            Some(cols) => (0..cols.len() as u8)
                .map(IconColor::Tier)
                .chain(std::iter::once(IconColor::Gray))
                .collect(),
            None => IconColor::ALL.to_vec(),
        };
        for color in colors {
            let rgb = font_rgb(
                color,
                self.dark_theme,
                shape,
                self.background,
                self.font,
                tiers,
            );
            for bright in [false, true] {
                // M11 闪烁：有背景时提亮背景，透明底提亮字色。
                let (fg, bg) = if bright {
                    if shape == IconShape::Transparent {
                        (brighten(rgb, BLINK_BRIGHTEN), None)
                    } else {
                        (rgb, Some(brighten(bg_rgb, BLINK_BRIGHTEN)))
                    }
                } else {
                    // P1-1：非透明形状常态档必须带底色（与 settings.rs 预览同式）。
                    (rgb, (shape != IconShape::Transparent).then_some(bg_rgb))
                };
                for text in &texts {
                    let rgba = render_text_bitmap(self.size, text, fg, bg, shape, &face);
                    self.map.insert((color, text.clone(), bright), rgba);
                }
            }
        }
    }

    /// 任何渲染输入变化（DPI 档 / 主题 / 形状 / 背景 / 字色 / 字体 / 分档表）
    /// 时重建缓存。
    #[allow(clippy::too_many_arguments)]
    pub fn rebuild(
        &mut self,
        size: u32,
        dark_theme: bool,
        shape: IconShape,
        background: IconBackground,
        font: IconFontColor,
        font_family: String,
        tiers: Option<Vec<RgbColor>>,
    ) {
        if size != self.size
            || dark_theme != self.dark_theme
            || shape != self.shape
            || background != self.background
            || font != self.font
            || font_family != self.font_family
            || tiers != self.tier_colors
        {
            self.size = size;
            self.dark_theme = dark_theme;
            self.shape = shape;
            self.background = background;
            self.font = font;
            self.font_family = font_family;
            self.tier_colors = tiers;
            self.fill();
        }
    }

    /// 取渲染位图；`bright` = 低电闪烁的提亮档。
    pub fn icon_for(&self, color: IconColor, text: &str, bright: bool) -> Option<tray_icon::Icon> {
        let rgba = self.map.get(&(color, text.to_string(), bright))?;
        tray_icon::Icon::from_rgba(rgba.clone(), self.size, self.size).ok()
    }
}

/// #5：由字形墨迹边界框计算「居中所需平移量」。
///
/// 墨迹判定阈值 0x10（BGRA 任意通道亮度）；无墨迹（空文本/字体回退失败）
/// 返回 (0, 0)。目标位置 = bbox 居中于位图：左上角平移到
/// ((size - w) / 2, (size - h) / 2)，对齐偏差允许 ±1px（整数除法余数）。
fn ink_center_shift(src: &[u8], size: i32) -> (i32, i32) {
    let stride = size as usize;
    let mut x0 = i32::MAX;
    let mut x1 = i32::MIN;
    let mut y0 = i32::MAX;
    let mut y1 = i32::MIN;
    for (i, px) in src.chunks_exact(4).enumerate() {
        let v = px[0].max(px[1]).max(px[2]) as i32;
        if v >= 0x10 {
            let (py, px_x) = ((i / stride) as i32, (i % stride) as i32);
            x0 = x0.min(px_x);
            x1 = x1.max(px_x);
            y0 = y0.min(py);
            y1 = y1.max(py);
        }
    }
    if x0 > x1 {
        return (0, 0);
    }
    let w = x1 - x0 + 1;
    let h = y1 - y0 + 1;
    ((size - w) / 2 - x0, (size - h) / 2 - y0)
}

/// GDI 渲染：黑底白字取覆盖率，再按「形状底 + 字色」直出 RGBA。
///
/// - `bg = None`（transparent 形状）：颜色 = 目标色，alpha = 字覆盖率；
/// - `bg = Some`：rounded 圆底（1px 软边缘）/ square 直角底（四边 1px 软边缘），
///   文字按 source-over 叠加在底色之上；
/// - `face` 字体缺失时 GDI 静默回退系统默认（M9 容错，不报错）；
/// - v0.3.3 #5：两遍渲染——先按估算位置落字，扫描字形墨迹边界框，
///   再平移到水平 + 垂直真正居中（按字形实际 bbox，非行高估算）；
///   字体质量用 ANTIALIASED_QUALITY（消除位图字模糊）。
pub fn render_text_bitmap(
    size: u32,
    text: &str,
    (r, g, b): (u8, u8, u8),
    bg: Option<(u8, u8, u8)>,
    shape: IconShape,
    face: &str,
) -> Vec<u8> {
    let n = (size * size) as usize;
    let mut rgba = vec![0u8; n * 4];
    let chars: Vec<char> = text.chars().collect();
    let nchars = chars.len() as i32;
    let size_i = size as i32;
    let font_h = if nchars >= 2 {
        size_i * 5 / 8
    } else {
        size_i * 3 / 4
    };
    // 数字宽度 ≈ 0.55 × 字高；CJK 宽度 ≈ 1.0 × 字高。
    let est_w: i32 = chars
        .iter()
        .map(|c| {
            if c.is_ascii() {
                font_h * 11 / 20
            } else {
                font_h
            }
        })
        .sum();
    let x = ((size_i - est_w) / 2).max(0);
    let y = ((size_i - font_h) / 2).max(0);
    let text_w: Vec<u16> = text.encode_utf16().collect();
    let face_w = wide(face);

    unsafe {
        let hdc = CreateCompatibleDC(None);
        if hdc.is_invalid() {
            return rgba;
        }
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: size_i,
                biHeight: -size_i, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            bmiColors: [RGBQUAD::default()],
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let Ok(hbmp) = CreateDIBSection(Some(hdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            let _ = DeleteDC(hdc);
            return rgba;
        };
        if !bits.is_null() {
            std::ptr::write_bytes(bits, 0, n * 4);
            let old = SelectObject(hdc, HGDIOBJ(hbmp.0));
            let font = CreateFontW(
                -font_h,
                0,
                0,
                0,
                700,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                Default::default(),
                Default::default(),
                ANTIALIASED_QUALITY,
                0,
                PCWSTR::from_raw(face_w.as_ptr()),
            );
            let old_font = SelectObject(hdc, HGDIOBJ(font.0));
            let _ = SetBkMode(hdc, TRANSPARENT);
            let _ = SetTextColor(hdc, windows::Win32::Foundation::COLORREF(0x00FF_FFFF));
            // 第一遍：按估算位置落字。
            let _ = TextOutW(hdc, x, y, &text_w);
            // #5：按字形实际墨迹边界框计算居中平移量。
            let probe = std::slice::from_raw_parts(bits as *const u8, n * 4);
            let (dx, dy) = ink_center_shift(probe, size_i);
            if dx != 0 || dy != 0 {
                // 第二遍：清空后在平移后的位置重画。
                std::ptr::write_bytes(bits, 0, n * 4);
                let _ = TextOutW(hdc, x + dx, y + dy, &text_w);
            }
            // BGRA → RGBA：取字覆盖度（亮度），与形状底色合成。
            let src = std::slice::from_raw_parts(bits as *const u8, n * 4);
            let cx = (size_i - 1) as f32 / 2.0;
            let radius = size_i as f32 / 2.0;
            for i in 0..n {
                let (py, px) = ((i / size as usize) as i32, (i % size as usize) as i32);
                let (bb, gg, rr) = (src[i * 4], src[i * 4 + 1], src[i * 4 + 2]);
                let t = bb.max(gg).max(rr) as f32 / 255.0;
                let (br, bgc, bb_, ba) = match bg {
                    Some((br_, bg__, bl_)) => {
                        // 形状覆盖度：圆底 = 内切圆 1px 软边缘；直角 = 四边 1px 软边缘。
                        let cover = match shape {
                            IconShape::Rounded => {
                                let dx = px as f32 - cx;
                                let dy = py as f32 - cx;
                                let dist = (dx * dx + dy * dy).sqrt();
                                (radius + 0.5 - dist).clamp(0.0, 1.0)
                            }
                            IconShape::Square => {
                                let edge = (px).min(py).min(size_i - 1 - px).min(size_i - 1 - py);
                                (edge as f32 + 0.5).clamp(0.0, 1.0)
                            }
                            IconShape::Transparent => 0.0,
                        };
                        (br_, bg__, bl_, cover)
                    }
                    None => (0u8, 0u8, 0u8, 0.0),
                };
                if t <= 0.0 && ba <= 0.0 {
                    continue;
                }
                // source-over：文字（颜色=字色，覆盖度 t）叠在底色上。
                let out_a = t + ba * (1.0 - t);
                if out_a <= 0.0 {
                    continue;
                }
                let blend_fg = t / out_a;
                let mix = |fc: u8, bc: u8| -> u8 {
                    ((fc as f32 * blend_fg + bc as f32 * (1.0 - blend_fg)) as u16).min(255) as u8
                };
                rgba[i * 4] = mix(r, br);
                rgba[i * 4 + 1] = mix(g, bgc);
                rgba[i * 4 + 2] = mix(b, bb_);
                rgba[i * 4 + 3] = (out_a * 255.0 + 0.5) as u8;
            }
            SelectObject(hdc, old_font);
            SelectObject(hdc, old);
            let _ = DeleteObject(HGDIOBJ(font.0));
        }
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        let _ = DeleteDC(hdc);
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;
    use volttray_app::config::RgbColor;

    #[test]
    fn text_rules() {
        assert_eq!(icon_text(Some(100), false), "满");
        assert_eq!(icon_text(Some(99), false), "99");
        assert_eq!(icon_text(Some(7), false), "7");
        assert_eq!(icon_text(None, false), "?");
        assert_eq!(icon_text(Some(40), true), "--");
    }

    #[test]
    fn color_thresholds_use_user_threshold() {
        // M12：红档跟随用户阈值（默认 20）。
        assert_eq!(IconColor::for_percent(80, false, 20), IconColor::Default);
        assert_eq!(IconColor::for_percent(51, false, 20), IconColor::Default);
        assert_eq!(IconColor::for_percent(50, false, 20), IconColor::Yellow);
        assert_eq!(IconColor::for_percent(20, false, 20), IconColor::Red);
        // 阈值调到 35：31 变红档；36 落在固定黄档（≤50）。
        assert_eq!(IconColor::for_percent(31, false, 35), IconColor::Red);
        assert_eq!(IconColor::for_percent(36, false, 35), IconColor::Yellow);
        // 阈值 50：黄档被红档覆盖（≤50 全红）。
        assert_eq!(IconColor::for_percent(45, false, 50), IconColor::Red);
        assert_eq!(IconColor::for_percent(90, true, 20), IconColor::Gray);
    }

    #[test]
    fn dpi_buckets() {
        assert_eq!(dpi_bucket_size(96), 16);
        assert_eq!(dpi_bucket_size(144), 24);
        assert_eq!(dpi_bucket_size(192), 32);
        // v0.3.3 #5：连续尺寸——125%（120 DPI）不再被钳到 16px 再拉伸。
        assert_eq!(dpi_bucket_size(120), 20);
        assert_eq!(dpi_bucket_size(168), 28);
    }

    #[test]
    fn auto_font_resolves_against_shape_and_background() {
        // 默认出厂组合：深色圆底 + 正常档白字。
        assert_eq!(
            font_rgb(
                IconColor::Default,
                true,
                IconShape::Rounded,
                IconBackground::Dark,
                IconFontColor::Auto,
                None
            ),
            (0xFF, 0xFF, 0xFF)
        );
        // 浅色底配深字。
        assert_eq!(
            font_rgb(
                IconColor::Default,
                true,
                IconShape::Rounded,
                IconBackground::Light,
                IconFontColor::Auto,
                None
            ),
            (0x1B, 0x1B, 0x1B)
        );
        // 透明形状回退主题对比。
        assert_eq!(
            font_rgb(
                IconColor::Default,
                true,
                IconShape::Transparent,
                IconBackground::Dark,
                IconFontColor::Auto,
                None
            ),
            (0xFF, 0xFF, 0xFF)
        );
        assert_eq!(
            font_rgb(
                IconColor::Default,
                false,
                IconShape::Transparent,
                IconBackground::Dark,
                IconFontColor::Auto,
                None
            ),
            (0x1B, 0x1B, 0x1B)
        );
        // 自定义底：黑白中取对比更高者（深紫底 → 白字）。
        assert_eq!(
            font_rgb(
                IconColor::Default,
                false,
                IconShape::Square,
                IconBackground::Custom(RgbColor {
                    r: 0x30,
                    g: 0x10,
                    b: 0x50
                }),
                IconFontColor::Auto,
                None
            ),
            (0xFF, 0xFF, 0xFF)
        );
        // 警告/低电档不随背景变化。
        assert_eq!(
            font_rgb(
                IconColor::Yellow,
                false,
                IconShape::Rounded,
                IconBackground::Dark,
                IconFontColor::Auto,
                None
            ),
            (0xFF, 0xD2, 0x1F)
        );
        assert_eq!(
            font_rgb(
                IconColor::Red,
                false,
                IconShape::Rounded,
                IconBackground::Light,
                IconFontColor::Auto,
                None
            ),
            (0xE8, 0x11, 0x23)
        );
    }

    #[test]
    fn tier_color_overrides_fixed_and_falls_back() {
        // 分档色优先于固定字色（#4：启用分档后单一颜色被忽略）。
        let tiers = vec![
            RgbColor {
                r: 0xE8,
                g: 0x11,
                b: 0x23,
            },
            RgbColor {
                r: 0xFF,
                g: 0xD2,
                b: 0x1F,
            },
            RgbColor {
                r: 0x00,
                g: 0xCC,
                b: 0x6A,
            },
        ];
        let fixed = IconFontColor::Fixed(RgbColor {
            r: 0x0A,
            g: 0x6C,
            b: 0xFF,
        });
        assert_eq!(
            font_rgb(
                IconColor::Tier(0),
                true,
                IconShape::Rounded,
                IconBackground::Dark,
                fixed,
                Some(&tiers)
            ),
            (0xE8, 0x11, 0x23)
        );
        assert_eq!(
            font_rgb(
                IconColor::Tier(2),
                true,
                IconShape::Rounded,
                IconBackground::Dark,
                fixed,
                Some(&tiers)
            ),
            (0x00, 0xCC, 0x6A)
        );
        // 档位越界（表缺失该档）→ 灰色兜底。
        assert_eq!(
            font_rgb(
                IconColor::Tier(9),
                true,
                IconShape::Rounded,
                IconBackground::Dark,
                fixed,
                Some(&tiers)
            ),
            (0x80, 0x80, 0x80)
        );
        // 未提供分档表的 Tier → 灰色兜底。
        assert_eq!(
            font_rgb(
                IconColor::Tier(0),
                true,
                IconShape::Rounded,
                IconBackground::Dark,
                fixed,
                None
            ),
            (0x80, 0x80, 0x80)
        );
        // 灰显不受分档影响。
        assert_eq!(
            font_rgb(
                IconColor::Gray,
                true,
                IconShape::Rounded,
                IconBackground::Dark,
                fixed,
                Some(&tiers)
            ),
            (0x80, 0x80, 0x80)
        );
    }

    #[test]
    fn fixed_color_overrides_levels_but_not_gray() {
        let fixed = IconFontColor::Fixed(RgbColor {
            r: 0x0A,
            g: 0x6C,
            b: 0xFF,
        });
        assert_eq!(
            font_rgb(
                IconColor::Default,
                false,
                IconShape::Rounded,
                IconBackground::Light,
                fixed,
                None
            ),
            (0x0A, 0x6C, 0xFF)
        );
        assert_eq!(
            font_rgb(
                IconColor::Yellow,
                false,
                IconShape::Rounded,
                IconBackground::Dark,
                fixed,
                None
            ),
            (0x0A, 0x6C, 0xFF)
        );
        // 灰显（未知?/离线--）保持灰色。
        assert_eq!(
            font_rgb(
                IconColor::Gray,
                false,
                IconShape::Rounded,
                IconBackground::Dark,
                fixed,
                None
            ),
            (0x80, 0x80, 0x80)
        );
    }

    /// P1-1 回归：非透明形状常态渲染必须带底色（防 == / != 再写反）。
    /// 采样点 (x=size/2, y=2)：圆底覆盖度=1 且在文字区（y 起点 6）之上，
    /// 输出必为纯 bg_rgb、alpha=255；透明形状同点 alpha 必为 0。
    #[test]
    fn fill_non_transparent_shape_keeps_background() {
        let size = 32u32;
        let bg = IconBackground::Dark;
        let cache = IconCache::new(
            size,
            true,
            IconShape::Rounded,
            bg,
            IconFontColor::Auto,
            "Segoe UI".to_string(),
            None,
        );
        let rgba = cache
            .map
            .get(&(IconColor::Default, "50".to_string(), false))
            .expect("normal-level bitmap cached");
        let idx = ((2 * size + size / 2) * 4) as usize;
        let (er, eg, eb) = bg.rgb();
        assert_eq!(&rgba[idx..idx + 3], &[er, eg, eb]);
        assert_eq!(rgba[idx + 3], 255);

        // 对照：透明形状同位置无背景。
        let transparent = IconCache::new(
            size,
            true,
            IconShape::Transparent,
            bg,
            IconFontColor::Auto,
            "Segoe UI".to_string(),
            None,
        );
        let rgba_t = transparent
            .map
            .get(&(IconColor::Default, "50".to_string(), false))
            .expect("transparent bitmap cached");
        assert_eq!(rgba_t[idx + 3], 0);
    }

    /// #5：文本按字形实际边界框水平 + 垂直居中——透明底（alpha = 覆盖率）
    /// 便于逐像素断言：bbox 中心与位图中心偏差 ≤ 1px，且墨迹越过中心线
    /// （拦住「偏左 / 偏上」回归）。
    #[test]
    fn text_is_centered_by_ink_bbox() {
        let size = 32u32;
        for text in ["50", "7", "满"] {
            let rgba = render_text_bitmap(
                size,
                text,
                (0xFF, 0xFF, 0xFF),
                None,
                IconShape::Transparent,
                "Segoe UI",
            );
            let center = ((size - 1) / 2) as i32;
            let mut x0 = i32::MAX;
            let mut x1 = i32::MIN;
            let mut y0 = i32::MAX;
            let mut y1 = i32::MIN;
            for (i, px) in rgba.chunks_exact(4).enumerate() {
                if px[3] >= 0x10 {
                    let (py, pxx) = ((i / size as usize) as i32, (i % size as usize) as i32);
                    x0 = x0.min(pxx);
                    x1 = x1.max(pxx);
                    y0 = y0.min(py);
                    y1 = y1.max(py);
                }
            }
            assert!(x0 <= x1, "{text}: no ink rendered");
            let cx = (x0 + x1) as f32 / 2.0;
            let cy = (y0 + y1) as f32 / 2.0;
            assert!(
                (cx - center as f32).abs() <= 1.0,
                "{text}: ink bbox center x {cx} vs {center}"
            );
            assert!(
                (cy - center as f32).abs() <= 1.0,
                "{text}: ink bbox center y {cy} vs {center}"
            );
            // 墨迹必须覆盖中心区域两侧（偏左/偏上直接被拦）。
            assert!(
                x0 <= center && x1 >= center,
                "{text}: not spanning center x"
            );
            assert!(
                y0 <= center && y1 >= center,
                "{text}: not spanning center y"
            );
        }
    }

    /// #5：抗锯齿渲染——字形边缘存在部分覆盖（0 < alpha < 255）像素。
    #[test]
    fn aa_renders_partial_coverage() {
        let size = 32u32;
        let rgba = render_text_bitmap(
            size,
            "50",
            (0xFF, 0xFF, 0xFF),
            None,
            IconShape::Transparent,
            "Segoe UI",
        );
        let partial = rgba
            .chunks_exact(4)
            .filter(|px| px[3] > 0 && px[3] < 255)
            .count();
        assert!(
            partial > 0,
            "anti-aliased edge pixels expected, got {partial}"
        );
        // 完全覆盖（实心）像素也存在（字形主体）。
        let solid = rgba.chunks_exact(4).filter(|px| px[3] == 255).count();
        assert!(solid > 0, "solid glyph core expected");
    }

    /// #4：分档模式缓存 —— N 个分档色 + 灰共 N+1 个颜色档位可用。
    #[test]
    fn tier_cache_fills_tier_colors() {
        let tiers = vec![
            RgbColor {
                r: 0xE8,
                g: 0x11,
                b: 0x23,
            },
            RgbColor {
                r: 0xFF,
                g: 0xD2,
                b: 0x1F,
            },
            RgbColor {
                r: 0x00,
                g: 0xCC,
                b: 0x6A,
            },
        ];
        let cache = IconCache::new(
            16,
            true,
            IconShape::Rounded,
            IconBackground::Dark,
            IconFontColor::Auto,
            "Segoe UI".to_string(),
            Some(tiers),
        );
        for i in 0..3u8 {
            assert!(
                cache
                    .map
                    .contains_key(&(IconColor::Tier(i), "50".to_string(), false)),
                "tier {i} bitmap missing"
            );
        }
        assert!(cache
            .map
            .contains_key(&(IconColor::Gray, "50".to_string(), false)));
        // 分档模式不渲染默认 4 档。
        assert!(!cache
            .map
            .contains_key(&(IconColor::Default, "50".to_string(), false)));
        assert!(!cache
            .map
            .contains_key(&(IconColor::Red, "50".to_string(), false)));
    }
}
