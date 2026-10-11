//! 托盘图标渲染与缓存（蓝图 7 + 评审 4.1 修订）。
//!
//! - 启动时按当前 DPI 档（100%/150%/200% → 16/24/32px）用 GDI 预渲染
//!   全部文本位图（"0".."99"、"满"、"?"、"--"）× 4 色，运行时只做像素缓存换图标，
//!   热路径零 GDI 调用。
//! - 两位数字封顶：100% 显示 "满"；未知 "?"；灰显 "--"。
//! - 颜色：>50 默认（随主题黑/白）、20..=50 黄、<=20 红、灰显灰。

use std::collections::HashMap;
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

    fn rgb(self, dark_theme: bool) -> (u8, u8, u8) {
        match self {
            Self::Default => {
                if dark_theme {
                    (0xFF, 0xFF, 0xFF)
                } else {
                    (0x1B, 0x1B, 0x1B)
                }
            }
            Self::Yellow => (0xFF, 0xD2, 0x1F),
            Self::Red => (0xE8, 0x11, 0x23),
            Self::Gray => (0x80, 0x80, 0x80),
        }
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

/// 预渲染位图缓存：key = (颜色, 文本)。
pub struct IconCache {
    pub size: u32,
    dark_theme: bool,
    map: HashMap<(IconColor, String), Vec<u8>>,
}

impl IconCache {
    pub fn new(size: u32, dark_theme: bool) -> Self {
        let mut cache = IconCache {
            size,
            dark_theme,
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
            let rgb = color.rgb(self.dark_theme);
            for text in &texts {
                let rgba = render_text_bitmap(self.size, text, rgb);
                self.map.insert((color, text.clone()), rgba);
            }
        }
    }

    pub fn rebuild(&mut self, size: u32, dark_theme: bool) {
        if size != self.size || dark_theme != self.dark_theme {
            self.size = size;
            self.dark_theme = dark_theme;
            self.fill();
        }
    }

    pub fn icon_for(&self, color: IconColor, text: &str) -> Option<tray_icon::Icon> {
        let rgba = self.map.get(&(color, text.to_string()))?;
        tray_icon::Icon::from_rgba(rgba.clone(), self.size, self.size).ok()
    }
}

/// GDI 渲染：黑底白字 → 逐像素 alpha = 亮度，颜色 = 目标色。
fn render_text_bitmap(size: u32, text: &str, (r, g, b): (u8, u8, u8)) -> Vec<u8> {
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
        let Ok(hbmp) = CreateDIBSection(hdc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
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
            // BGRA → RGBA（alpha = 亮度，颜色 = 目标色）。
            let src = std::slice::from_raw_parts(bits as *const u8, n * 4);
            for i in 0..n {
                let (bb, gg, rr) = (src[i * 4], src[i * 4 + 1], src[i * 4 + 2]);
                let alpha = bb.max(gg).max(rr);
                if alpha > 0 {
                    rgba[i * 4] = r;
                    rgba[i * 4 + 1] = g;
                    rgba[i * 4 + 2] = b;
                    rgba[i * 4 + 3] = alpha;
                }
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
}
