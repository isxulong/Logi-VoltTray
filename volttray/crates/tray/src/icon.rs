//! 托盘图标渲染与缓存（蓝图 7 + 评审 4.1 修订 + v0.2.0 可读性）。
//!
//! - 启动时按当前 DPI 档（100%/150%/200% → 16/24/32px）用 GDI 预渲染
//!   全部文本位图（"0".."99"、"满"、"?"、"--"）× 4 色，运行时只做像素缓存换图标，
//!   热路径零 GDI 调用。
//! - 两位数字封顶：100% 显示 "满"；未知 "?"；灰显 "--"。
//! - 颜色：>50 默认、20..=50 黄、<=20 红、灰显灰；默认档颜色依背景/主题取高对比。
//! - v0.2.0 可读性：图标背景（transparent/light/dark，默认 dark 深色圆底）+
//!   字体颜色（auto / #RRGGBB）。出厂组合「深色圆底 + 依背景高对比字色」保证
//!   在深色与浅色任务栏上都清晰可读（真机反馈：无背景黑字不清晰）。

use std::collections::HashMap;
use volttray_app::config::{IconBackground, IconFontColor};
use windows::core::PCWSTR;
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject, SelectObject,
    SetBkMode, SetTextColor, TextOutW, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DEFAULT_CHARSET,
    DIB_RGB_COLORS, HGDIOBJ, RGBQUAD, TRANSPARENT,
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
}

impl IconColor {
    const ALL: [IconColor; 4] = [Self::Default, Self::Yellow, Self::Red, Self::Gray];

    /// 按电量选色（蓝图 7）。
    pub fn for_percent(percent: u8, gray: bool) -> Self {
        if gray {
            Self::Gray
        } else if percent <= 20 {
            Self::Red
        } else if percent <= 50 {
            Self::Yellow
        } else {
            Self::Default
        }
    }
}

/// 解析某电量档的字体 RGB（v0.2.0 可读性）。
///
/// - 灰显（未知/离线）固定灰色，不受固定色覆盖；
/// - 固定色覆盖正常/警告/严重档；
/// - auto：警告黄、严重红、正常档依背景取高对比（深底白字/浅底深字/透明按主题）。
pub fn font_rgb(
    level: IconColor,
    dark_theme: bool,
    bg: IconBackground,
    font: IconFontColor,
) -> (u8, u8, u8) {
    match (font, level) {
        // 灰显永远是灰色：未知(?)与离线(--)不参与配色覆盖。
        (_, IconColor::Gray) => (0x80, 0x80, 0x80),
        (IconFontColor::Fixed(c), _) => (c.r, c.g, c.b),
        (IconFontColor::Auto, IconColor::Yellow) => (0xFF, 0xD2, 0x1F),
        (IconFontColor::Auto, IconColor::Red) => (0xE8, 0x11, 0x23),
        (IconFontColor::Auto, IconColor::Default) => match bg {
            IconBackground::Dark => (0xFF, 0xFF, 0xFF),
            IconBackground::Light => (0x1B, 0x1B, 0x1B),
            IconBackground::Transparent => {
                if dark_theme {
                    (0xFF, 0xFF, 0xFF)
                } else {
                    (0x1B, 0x1B, 0x1B)
                }
            }
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
    if dpi >= 192 {
        32
    } else if dpi >= 144 {
        24
    } else {
        16
    }
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

/// 预渲染位图缓存：key = (颜色档, 文本)。
pub struct IconCache {
    pub size: u32,
    dark_theme: bool,
    background: IconBackground,
    font: IconFontColor,
    map: HashMap<(IconColor, String), Vec<u8>>,
}

impl IconCache {
    pub fn new(
        size: u32,
        dark_theme: bool,
        background: IconBackground,
        font: IconFontColor,
    ) -> Self {
        let mut cache = IconCache {
            size,
            dark_theme,
            background,
            font,
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
        for color in IconColor::ALL {
            let rgb = font_rgb(color, self.dark_theme, self.background, self.font);
            for text in &texts {
                let rgba = render_text_bitmap(self.size, text, rgb, self.background.rgb());
                self.map.insert((color, text.clone()), rgba);
            }
        }
    }

    /// 任何渲染输入变化（DPI 档 / 主题 / 背景设置 / 字色设置）时重建缓存。
    pub fn rebuild(
        &mut self,
        size: u32,
        dark_theme: bool,
        background: IconBackground,
        font: IconFontColor,
    ) {
        if size != self.size
            || dark_theme != self.dark_theme
            || background != self.background
            || font != self.font
        {
            self.size = size;
            self.dark_theme = dark_theme;
            self.background = background;
            self.font = font;
            self.fill();
        }
    }

    pub fn icon_for(&self, color: IconColor, text: &str) -> Option<tray_icon::Icon> {
        let rgba = self.map.get(&(color, text.to_string()))?;
        tray_icon::Icon::from_rgba(rgba.clone(), self.size, self.size).ok()
    }
}

/// GDI 渲染：黑底白字取覆盖率，再按「背景圆底 + 字色」直出 RGBA。
///
/// - 背景为 None（transparent）时保持旧版行为：颜色 = 目标色，alpha = 字覆盖率；
/// - 有背景时：整圆底色（1px 软边缘），文字按 source-over 叠加在底色之上。
fn render_text_bitmap(
    size: u32,
    text: &str,
    (r, g, b): (u8, u8, u8),
    bg: Option<(u8, u8, u8)>,
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
    let face = wide("Segoe UI");

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
                Default::default(),
                0,
                PCWSTR::from_raw(face.as_ptr()),
            );
            let old_font = SelectObject(hdc, HGDIOBJ(font.0));
            let _ = SetBkMode(hdc, TRANSPARENT);
            let _ = SetTextColor(hdc, windows::Win32::Foundation::COLORREF(0x00FF_FFFF));
            let _ = TextOutW(hdc, x, y, &text_w);
            // BGRA → RGBA：取字覆盖度（亮度），与背景圆底合成。
            let src = std::slice::from_raw_parts(bits as *const u8, n * 4);
            let cx = (size_i - 1) as f32 / 2.0;
            let radius = size_i as f32 / 2.0;
            for i in 0..n {
                let (py, px) = ((i / size as usize) as i32, (i % size as usize) as i32);
                let (bb, gg, rr) = (src[i * 4], src[i * 4 + 1], src[i * 4 + 2]);
                let t = bb.max(gg).max(rr) as f32 / 255.0;
                let (br, bgc, bb_, ba) = match bg {
                    Some((br_, bg__, bl_)) => {
                        // 1px 软边缘圆底。
                        let dx = px as f32 - cx;
                        let dy = py as f32 - cx;
                        let dist = (dx * dx + dy * dy).sqrt();
                        let cover = (radius + 0.5 - dist).clamp(0.0, 1.0);
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
    fn color_thresholds() {
        assert_eq!(IconColor::for_percent(80, false), IconColor::Default);
        assert_eq!(IconColor::for_percent(51, false), IconColor::Default);
        assert_eq!(IconColor::for_percent(50, false), IconColor::Yellow);
        assert_eq!(IconColor::for_percent(20, false), IconColor::Red);
        assert_eq!(IconColor::for_percent(90, true), IconColor::Gray);
    }

    #[test]
    fn dpi_buckets() {
        assert_eq!(dpi_bucket_size(96), 16);
        assert_eq!(dpi_bucket_size(144), 24);
        assert_eq!(dpi_bucket_size(192), 32);
    }

    #[test]
    fn auto_font_resolves_against_background() {
        // 默认出厂组合：深色底 + 正常档白字 → 深浅任务栏均可读。
        assert_eq!(
            font_rgb(IconColor::Default, true, IconBackground::Dark, IconFontColor::Auto),
            (0xFF, 0xFF, 0xFF)
        );
        assert_eq!(
            font_rgb(IconColor::Default, false, IconBackground::Dark, IconFontColor::Auto),
            (0xFF, 0xFF, 0xFF)
        );
        // 浅色底配深字。
        assert_eq!(
            font_rgb(IconColor::Default, true, IconBackground::Light, IconFontColor::Auto),
            (0x1B, 0x1B, 0x1B)
        );
        // 透明底回退主题对比（旧版行为）。
        assert_eq!(
            font_rgb(IconColor::Default, true, IconBackground::Transparent, IconFontColor::Auto),
            (0xFF, 0xFF, 0xFF)
        );
        assert_eq!(
            font_rgb(IconColor::Default, false, IconBackground::Transparent, IconFontColor::Auto),
            (0x1B, 0x1B, 0x1B)
        );
        // 警告/严重档不随背景变化。
        assert_eq!(
            font_rgb(IconColor::Yellow, false, IconBackground::Dark, IconFontColor::Auto),
            (0xFF, 0xD2, 0x1F)
        );
        assert_eq!(
            font_rgb(IconColor::Red, false, IconBackground::Light, IconFontColor::Auto),
            (0xE8, 0x11, 0x23)
        );
    }

    #[test]
    fn fixed_color_overrides_levels_but_not_gray() {
        let fixed = IconFontColor::Fixed(RgbColor { r: 0x0A, g: 0x6C, b: 0xFF });
        assert_eq!(
            font_rgb(IconColor::Default, false, IconBackground::Light, fixed),
            (0x0A, 0x6C, 0xFF)
        );
        assert_eq!(
            font_rgb(IconColor::Yellow, false, IconBackground::Dark, fixed),
            (0x0A, 0x6C, 0xFF)
        );
        assert_eq!(
            font_rgb(IconColor::Red, false, IconBackground::Dark, fixed),
            (0x0A, 0x6C, 0xFF)
        );
        // 灰显（未知?/离线--）保持灰色。
        assert_eq!(
            font_rgb(IconColor::Gray, false, IconBackground::Dark, fixed),
            (0x80, 0x80, 0x80)
        );
    }
}
