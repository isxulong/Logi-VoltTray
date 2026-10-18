//! 托盘图标渲染与缓存（v0.3.0 设置体系 M9/M11/M12）。
//!
//! - v0.3.15（P1-2）：懒渲染 + 小容量 LRU——启动不再全量预渲染
//!   （旧实现 4 色档 × 103 文本 × 2 提亮档 = 824 帧 RGBA 常驻，
//!   16px≈0.8MB / 32px≈3.3MB / 48px≈7.4MB 堆）；帧在首次被请求时
//!   渲染并缓存，容量上限见 `FRAME_CACHE_CAP` 论证；DPI / 渲染输入
//!   变更时整体清缓存。闪烁两相位帧命中缓存，每 500ms 不重渲染。
//! - 两位数字封顶：100% 显示 "满"；未知 "?"；灰显 "--"。
//! - 颜色档（M12）：≤ 用户阈值 红、≤ 50 黄（固定）、其余默认；灰显灰。
//! - 形状（M9）：rounded 圆底（默认）/ square 直角 / transparent 透明无背景。
//! - 背景色（M9）：light/dark 预设或 #RRGGBB 自定义；透明语义由形状承担。
//! - 字体（M9）：系统字体名（默认 Segoe UI），GDI 缺字体时静默回退。
//! - 闪烁（M11）：低电触发时在常态与「提亮帧」两档间 1Hz 切换；
//!   v0.3.4 #3 提亮帧改用 flash_rgb（向白/向深灰插值），两帧必有可感知差异；
//!   v0.3.5 #4（D-55）：透明形状的提亮帧走「文本帧路径」——字色跳变 +
//!   文本透明度脉冲（背景不存在，闪烁必须作用于文本）。
//! - HICON 构造（v0.3.4 #1 根因修复，详见 rgba_to_hicon 注释）。

use crate::frame_lru::FrameLru;
use volttray_app::config::{
    contrast_ratio, flash_rgb, pulse_alpha, Config, IconBackground, IconFontColor, IconShape,
    RgbColor, BLINK_TEXT_ALPHA_PULSE, ICON_BG_SCALE_MAX, ICON_BG_SCALE_MIN, ICON_TEXT_SCALE_MAX,
    ICON_TEXT_SCALE_MIN,
};
use windows::core::PCWSTR;
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateCompatibleDC, CreateDIBSection, CreateFontW, DeleteDC, DeleteObject,
    SelectObject, SetBkMode, SetTextColor, TextOutW, ANTIALIASED_QUALITY, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DEFAULT_CHARSET, DIB_RGB_COLORS, HBITMAP, HGDIOBJ, RGBQUAD,
    TRANSPARENT,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE,
    REG_VALUE_TYPE,
};
use windows::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, ICONINFO};

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

/// 单帧获取失败的具体原因（v0.3.8 D-64/B-1：渲染失败定位与启动回退用）。
///
/// 启动容错原则（B-1）：图标渲染失败永远不能杀死程序——失败原因必须
/// 落到 bootlog（哪个参数 / 哪个 GDI 阶段），随后用出厂默认渲染配置重试。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// RGBA 缓冲长度与 size×size×4 不符（渲染管线内部不变量被破坏）。
    BadBufferSize { expected: usize, actual: usize },
    /// CreateCompatibleDC 失败（GDI 系统级）。
    DcCreateFailed,
    /// CreateDIBSection 失败（GDI 系统级）。
    DibCreateFailed,
    /// CreateIconIndirect 失败（GDI 系统级）。
    IconCreateFailed,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::BadBufferSize { expected, actual } => write!(
                f,
                "RGBA 缓冲长度不符（期望 {expected} 字节，实际 {actual}）"
            ),
            FrameError::DcCreateFailed => write!(f, "CreateCompatibleDC 失败（GDI 系统级）"),
            FrameError::DibCreateFailed => write!(f, "CreateDIBSection 失败（GDI 系统级）"),
            FrameError::IconCreateFailed => write!(f, "CreateIconIndirect 失败（GDI 系统级）"),
        }
    }
}

/// v0.3.8（D-64 根因修复）：启动首帧（未知电量 "?"）的颜色档。
///
/// 必须与 pump 的 `percent == None` 分支保持**同一决策**：分档启用走
/// Gray 档，否则走 Default 档。v0.3.5 D-56 迁移把「分档关 + 字色 auto」
/// 的旧配置迁成分档开启后，fill 的键集合不再含 `IconColor::Default`——
/// 而 v0.3.4 以来的启动代码固定查 Default 档，启动即
/// 「default frame missing」退出（真机日志 stage: icon-cache）。
/// （v0.3.15：懒渲染后任何被请求的帧都现场渲染，「fill 键集合缺失」
/// 这一类故障在结构上不再可能；本函数保留以维持两路径同决策。）
pub fn startup_frame_color(tiers_enabled: bool) -> IconColor {
    if tiers_enabled {
        IconColor::Gray
    } else {
        IconColor::Default
    }
}

/// v0.3.8（B-1）：从配置视图构建图标缓存——main 启动与「出厂默认渲染
/// 配置重试」共用同一路径，保证回退后的缓存与默认配置渲染完全一致。
pub fn cache_from_config(cfg: &Config, size: u32, dark: bool) -> IconCache {
    IconCache::new(
        size,
        dark,
        cfg.icon_shape,
        cfg.icon_background,
        cfg.icon_font_color,
        cfg.icon_font_family.clone(),
        if cfg.icon_font_tiers_enabled {
            Some(cfg.icon_font_tier_colors.clone())
        } else {
            None
        },
        cfg.icon_text_scale,
        cfg.icon_bg_scale,
    )
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

/// 闪烁提亮帧的插值系数（M11：约 40%；v0.3.4 #3 改为 flash_rgb 插值）。
const BLINK_BRIGHTEN: f64 = 0.4;

/// 帧缓存容量上限（v0.3.15，P1-2 懒渲染 + LRU）。
///
/// 论证：同时可见帧数 = 各展示设备当前帧（托盘只展示 1 台设备的图标，
/// 加上设置窗口预览，常态 ≤2 帧）+ 低电闪烁的提亮相位帧（每台闪烁
/// 设备 +1）+ 电量变化过程中短期并存的新旧文本帧。32 帧对以上场景
/// 留有数量级余量；超出时 LRU 淘汰最久未命中的帧，重新显示时单次
/// 重渲染（~ms 级，且不在每 500ms 的闪烁热路径上——两相位帧常驻
/// 命中，`alternating_blink_phases_stay_cached` 锁定该语义）。
const FRAME_CACHE_CAP: usize = 32;

/// 位图帧缓存（v0.3.15：懒渲染 + LRU）：key = (颜色档, 文本, 提亮档)。
pub struct IconCache {
    pub size: u32,
    dark_theme: bool,
    shape: IconShape,
    background: IconBackground,
    font: IconFontColor,
    font_family: String,
    /// 分档颜色表（v0.3.3 #4）；None = 未启用分档（按默认/警告/低电档配色）。
    tier_colors: Option<Vec<RgbColor>>,
    /// 文本大小（% 相对基准字号，D-48/D-57，40..=160）。
    text_scale: u8,
    /// 背景覆盖度（% 相对位图内切块，D-48/D-57，20..=100；100% = 满出血）。
    bg_scale: u8,
    frames: FrameLru<(IconColor, String, bool)>,
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
        text_scale: u8,
        bg_scale: u8,
    ) -> Self {
        IconCache {
            size,
            dark_theme,
            shape,
            background,
            font,
            font_family,
            tier_colors: tiers,
            text_scale: text_scale.clamp(ICON_TEXT_SCALE_MIN, ICON_TEXT_SCALE_MAX),
            bg_scale: bg_scale.clamp(ICON_BG_SCALE_MIN, ICON_BG_SCALE_MAX),
            frames: FrameLru::new(FRAME_CACHE_CAP),
        }
    }

    /// 按需渲染单帧（v0.3.15：fill 的渲染内核对单帧抽提，配色/提亮/脉冲
    /// 逻辑与旧全量预渲染逐帧一致；调用方负责缓存键与 LRU 管理）。
    fn render_frame(&self, color: IconColor, text: &str, bright: bool) -> Vec<u8> {
        let tiers = self.tier_colors.as_deref();
        let rgb = font_rgb(
            color,
            self.dark_theme,
            self.shape,
            self.background,
            self.font,
            tiers,
        );
        let bg_rgb = self.background.rgb();
        // M11 闪烁：有背景时提亮背景，透明底提亮字色；
        // v0.3.4 #3：flash_rgb 保证提亮帧与常态帧必有可见差异。
        let (fg, bg) = if bright {
            if self.shape == IconShape::Transparent {
                (flash_rgb(rgb, BLINK_BRIGHTEN), None)
            } else {
                (rgb, Some(flash_rgb(bg_rgb, BLINK_BRIGHTEN)))
            }
        } else {
            // P1-1：非透明形状常态档必须带底色（与 settings.rs 预览同式）。
            (
                rgb,
                (self.shape != IconShape::Transparent).then_some(bg_rgb),
            )
        };
        let mut rgba = render_text_bitmap(
            self.size,
            text,
            fg,
            bg,
            self.shape,
            &self.font_family,
            self.text_scale,
            self.bg_scale,
        );
        // v0.3.5 #4（D-55）：透明背景的闪烁帧除字色跳变外再叠
        // 文本透明度脉冲——颜色对比弱（如深灰字）时帧间仍肉眼可见。
        if bright && self.shape == IconShape::Transparent {
            apply_alpha_pulse(&mut rgba, BLINK_TEXT_ALPHA_PULSE);
        }
        rgba
    }

    /// v0.3.18（U-5，单一渲染源）：设置窗预览直接取用真实托盘图标的
    /// 常态帧位图——与任务栏同一函数、同一渲染结果，预览不再维护
    /// 第二套相似实现。固定 bright=false（常态帧；R4 BF-9：闪烁提亮帧
    /// 不是用户在任务栏看到的常态，预览呈现常态才有对照意义）。
    pub fn render_frame_rgba(&self, color: IconColor, text: &str) -> Vec<u8> {
        self.render_frame(color, text, false)
    }

    /// 任何渲染输入变化（DPI 档 / 主题 / 形状 / 背景 / 字色 / 字体 / 分档表 /
    /// 文本与背景缩放）时重建缓存。
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
        text_scale: u8,
        bg_scale: u8,
    ) {
        let text_scale = text_scale.clamp(ICON_TEXT_SCALE_MIN, ICON_TEXT_SCALE_MAX);
        let bg_scale = bg_scale.clamp(ICON_BG_SCALE_MIN, ICON_BG_SCALE_MAX);
        if size != self.size
            || dark_theme != self.dark_theme
            || shape != self.shape
            || background != self.background
            || font != self.font
            || font_family != self.font_family
            || tiers != self.tier_colors
            || text_scale != self.text_scale
            || bg_scale != self.bg_scale
        {
            self.size = size;
            self.dark_theme = dark_theme;
            self.shape = shape;
            self.background = background;
            self.font = font;
            self.font_family = font_family;
            self.tier_colors = tiers;
            self.text_scale = text_scale;
            self.bg_scale = bg_scale;
            // v0.3.15（P1-2）：渲染输入已变 → 旧帧（尺寸/配色全部失效）
            // 整体清空，后续请求按新输入懒渲染。
            self.frames.clear();
        }
    }

    /// 取渲染图标；`bright` = 低电闪烁的提亮档。
    ///
    /// v0.3.4 #1 根因修复：不再用 tray-icon `Icon::from_rgba`（其内部
    /// `CreateIcon` 以非预乘 32bpp **DDB** 作颜色平面——Win32 只对 DIB 的
    /// 32bpp alpha 负责且要求预乘，DDB 通道被曲解导致自定义背景色偏差；
    /// 同时其 AND 掩码按逐像素整字节打包，1bpp 行掩码完全错误）。
    /// 改为 `rgba_to_hicon`：CreateDIBSection 预乘 BGRA + 规范 1bpp 掩码 +
    /// CreateIconIndirect，再走公开接口 `Icon::from_handle`（托盘销毁时
    /// DestroyIcon 由 tray-icon 负责）。
    pub fn icon_for(
        &mut self,
        color: IconColor,
        text: &str,
        bright: bool,
    ) -> Option<tray_icon::Icon> {
        self.icon_for_checked(color, text, bright).ok()
    }

    /// v0.3.8（D-64/B-1）：取帧并给出失败的具体原因（哪个参数 / 哪个
    /// GDI 阶段）——启动路径用本接口，失败原因可直接落 bootlog。
    ///
    /// v0.3.15（P1-2）：帧懒渲染——缓存未命中时现场渲染并入 LRU（命中
    /// 时零 GDI 调用，闪烁两相位帧因此每 500ms 不重渲染）。
    pub fn icon_for_checked(
        &mut self,
        color: IconColor,
        text: &str,
        bright: bool,
    ) -> Result<tray_icon::Icon, FrameError> {
        let key = (color, text.to_string(), bright);
        // contains 不推进 LRU 命中时钟；未命中才渲染，避免命中路径的
        // 任何 GDI 调用。
        let rendered = if self.frames.contains(&key) {
            None
        } else {
            Some(self.render_frame(color, text, bright))
        };
        let rgba = self
            .frames
            .get_or_insert_with(key, || rendered.expect("miss renders on demand"));
        let handle = rgba_to_hicon(rgba, self.size)?;
        Ok(tray_icon::Icon::from_handle(handle))
    }

    /// 测试专用：直接取缓存帧字节（blink.rs 的闪烁帧路径验收等跨模块测试用）。
    /// 未命中时按懒渲染语义现场渲染。
    #[cfg(test)]
    pub fn map_for_test(&mut self, color: IconColor, text: &str, bright: bool) -> Option<&[u8]> {
        let key = (color, text.to_string(), bright);
        let rendered = if self.frames.contains(&key) {
            None
        } else {
            Some(self.render_frame(color, text, bright))
        };
        Some(
            self.frames
                .get_or_insert_with(key, || rendered.expect("miss renders on demand")),
        )
    }

    /// 测试专用：当前缓存帧数（懒渲染/LRU 行为验收用）。
    #[cfg(test)]
    pub fn cached_len_for_test(&self) -> usize {
        self.frames.len()
    }
}

/// 整体透明度脉冲（v0.3.5 #4，D-55）：每个像素的 alpha × (1 − factor)，
/// 仅用于透明形状的闪烁提亮帧（文本帧路径）。
fn apply_alpha_pulse(rgba: &mut [u8], factor: f64) {
    for px in rgba.chunks_exact_mut(4) {
        px[3] = pulse_alpha(px[3], factor);
    }
}

/// v0.3.18（U-4）：扫描字形墨迹边界框（替代 ink_center_shift 的「平移量」
/// 形式——探针流程需要的是 bbox 本身：既判定真实墨迹是否超出背景块，
/// 也用作输出位图开窗居中的锚点）。
///
/// 墨迹判定阈值 0x10（BGRA 任意通道亮度）；无墨迹（空文本/字体回退失败）
/// 返回 None。有墨迹返回闭区间 (x0, y0, x1, y1)。
fn ink_bbox(src: &[u8], size: i32) -> Option<(i32, i32, i32, i32)> {
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
    (x0 <= x1).then_some((x0, y0, x1, y1))
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
#[allow(clippy::too_many_arguments)] // 8 参数：渲染管线固有输入，v0.3.4 #4 追加 text_scale/bg_scale
pub fn render_text_bitmap(
    size: u32,
    text: &str,
    (r, g, b): (u8, u8, u8),
    bg: Option<(u8, u8, u8)>,
    shape: IconShape,
    face: &str,
    text_scale: u8,
    bg_scale: u8,
) -> Vec<u8> {
    let n = (size * size) as usize;
    let mut rgba = vec![0u8; n * 4];
    let chars: Vec<char> = text.chars().collect();
    let nchars = chars.len() as i32;
    let size_i = size as i32;
    // v0.3.4 #4（D-48）：文本/背景缩放。100% 档与 v0.3.3 渲染逐字节一致。
    // ts/bs 先钳制；背景块 block = 位图 × bs%（transparent 恒满幅）；
    // 文本高度上限 cap = block × 15/16（背景块四周留 1px），超宽再按比例缩。
    let ts = (text_scale as i32).clamp(ICON_TEXT_SCALE_MIN as i32, ICON_TEXT_SCALE_MAX as i32);
    let bs = (bg_scale as i32).clamp(ICON_BG_SCALE_MIN as i32, ICON_BG_SCALE_MAX as i32);
    let block = if matches!(shape, IconShape::Transparent) {
        size_i
    } else {
        size_i * bs / 100
    };
    let cap = (block * 15 / 16).max(1);
    let base_h = if nchars >= 2 {
        size_i * 5 / 8
    } else {
        size_i * 3 / 4
    };
    let mut font_h = (base_h * ts / 100).max(1).min(cap);
    // 数字宽度 ≈ 0.55 × 字高；CJK 宽度 ≈ 1.0 × 字高。估算字宽只用于
    // 首轮候选字高：超宽先按比例缩字作起点——估算对粗字重字体系统性
    // 偏小（U-4 根因），最终是否放得下由下方探针循环按真实墨迹判定。
    let mut est_w: i32 = chars
        .iter()
        .map(|c| {
            if c.is_ascii() {
                font_h * 11 / 20
            } else {
                font_h
            }
        })
        .sum();
    // 超宽：逐档递减字高直至估算宽度放进安全边。R4 BF-1：旧实现按
    // cap/est_w 比例缩字，整数截断下非单调（size=24 时 ts=160 缩到 20
    // 反比 ts=140 的 21 更矮，字高随档位变大反而变小）；逐档递减从
    // 「不超过 ts 对应字高」的起点只向下走，保证最终字高随 ts 单调不减。
    while est_w > cap && font_h > 1 {
        font_h -= 1;
        est_w = chars
            .iter()
            .map(|c| {
                if c.is_ascii() {
                    font_h * 11 / 20
                } else {
                    font_h
                }
            })
            .sum();
    }
    // v0.3.18（U-4）：探针画布边长 = 位图 + 2×cap。旧流程按估算位置直接
    // 在输出位图上落字，再按墨迹 bbox 平移修正——估算偏小时文字已被
    // 位图边缘裁切，bbox 扫到的是残缺墨迹，修正无效（图2：Arial Black
    // @160% 底部裁切、未居中）。改为在放大的探针画布上真实落字、量
    // 真实墨迹、按 bbox 居中开窗拷贝回输出位图，从根上消除对估算精度
    // 的依赖。
    let ps = size_i + 2 * cap;
    let ps_n = (ps * ps) as usize;
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
                biWidth: ps,
                biHeight: -ps, // top-down
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
            let old = SelectObject(hdc, HGDIOBJ(hbmp.0));
            let _ = SetBkMode(hdc, TRANSPARENT);
            let _ = SetTextColor(hdc, windows::Win32::Foundation::COLORREF(0x00FF_FFFF));
            // 探针循环（U-4）：候选字高在探针画布上真实落字 → 扫真实墨迹
            // bbox → 超过背景块即按比例缩字重试。步进 (font_h/8).max(1)，
            // 粗字体 2~4 次收敛；32 次迭代保底（font_h 到 1 必停）。
            // 收敛后墨迹 bbox 必然 ≤ block ≤ size——预览（U-5 单一渲染源后
            // 即本函数产物）与任务栏都不会再出现裁切。
            let mut ink: Option<(i32, i32, i32, i32)> = None;
            for _ in 0..32 {
                std::ptr::write_bytes(bits, 0, ps_n * 4);
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
                let cur_est_w: i32 = chars
                    .iter()
                    .map(|c| {
                        if c.is_ascii() {
                            font_h * 11 / 20
                        } else {
                            font_h
                        }
                    })
                    .sum();
                // 候选位置：按估算宽高在探针画布居中（估算偏小只会让墨迹
                // 略偏，bbox 扫描会量出真实位置，不影响结果正确性）。
                let _ = TextOutW(hdc, (ps - cur_est_w) / 2, (ps - font_h) / 2, &text_w);
                SelectObject(hdc, old_font);
                let _ = DeleteObject(HGDIOBJ(font.0));
                let probe = std::slice::from_raw_parts(bits as *const u8, ps_n * 4);
                ink = ink_bbox(probe, ps);
                let fits = ink.is_some_and(|(x0, y0, x1, y1)| {
                    x1 - x0 + 1 <= block && y1 - y0 + 1 <= block
                });
                // 无墨迹（空文本/字体回退失败）或已放进背景块：收工；
                // font_h 到 1 仍放不下的极端情形接受当前结果——输出按
                // bbox 居中开窗拷贝，超出部分裁掉但保持居中（旧行为是
                // 既裁切又偏移，此处严格更优）。
                if fits || ink.is_none() || font_h <= 1 {
                    break;
                }
                font_h = (font_h - (font_h / 8).max(1)).max(1);
            }
            // BGRA → RGBA：以墨迹 bbox 中心为锚，在探针上开 size×size
            // 窗口取字覆盖度（亮度），与形状底色合成。
            let src = std::slice::from_raw_parts(bits as *const u8, ps_n * 4);
            let (off_x, off_y) = ink.map_or((0, 0), |(x0, y0, x1, y1)| {
                ((x0 + x1 + 1 - size_i) / 2, (y0 + y1 + 1 - size_i) / 2)
            });
            let has_ink = ink.is_some();
            let cx = (size_i - 1) as f32 / 2.0;
            // v0.3.4 #4：背景块几何基于 block（100% 时与 v0.3.3 相同）。
            let radius = block as f32 / 2.0;
            let lo = (size_i - block) / 2;
            let hi = lo + block - 1;
            for i in 0..n {
                let (py, px) = ((i / size as usize) as i32, (i % size as usize) as i32);
                // 开窗拷贝：输出像素 (px,py) 对应探针 (px+off_x, py+off_y)，
                // bbox 中心恰好落在输出位图中心（cx）；越界视为无墨迹。
                let (qx, qy) = (px + off_x, py + off_y);
                let t = if has_ink && qx >= 0 && qy >= 0 && qx < ps && qy < ps {
                    let s = ((qy * ps + qx) * 4) as usize;
                    src[s].max(src[s + 1]).max(src[s + 2]) as f32 / 255.0
                } else {
                    0.0
                };
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
                                let edge = (px - lo).min(py - lo).min(hi - px).min(hi - py);
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
            SelectObject(hdc, old);
        }
        let _ = DeleteObject(HGDIOBJ(hbmp.0));
        let _ = DeleteDC(hdc);
    }
    rgba
}

/// RGBA（直 alpha）→ 预乘 BGRA 字节序（Win32 32bpp DIB 语义）。
///
/// v0.3.4 #1 根因修复的一部分：Win32 只对 DIB 的 32bpp alpha 生效，且要求
/// **预乘**（premultiplied）BGRA——即 `B = B×A/255`。直 alpha 写入会被
/// GDI/Shell 当作已预乘解释，导致 RGB 被压缩、颜色发灰发暗。
/// 纯函数，可跨平台单测。
pub fn premultiplied_bgra_from_rgba(rgba: &[u8]) -> Vec<u8> {
    debug_assert!(rgba.len().is_multiple_of(4));
    let mut out = vec![0u8; rgba.len()];
    for (o, px) in out.chunks_exact_mut(4).zip(rgba.chunks_exact(4)) {
        let a = px[3] as u32;
        // 预乘：四舍五入，a=255 时精确等于原值（纯红 (255,0,0,255) → BGRA [0,0,255,255]）。
        o[0] = ((px[2] as u32 * a + 127) / 255) as u8;
        o[1] = ((px[1] as u32 * a + 127) / 255) as u8;
        o[2] = ((px[0] as u32 * a + 127) / 255) as u8;
        o[3] = px[3];
    }
    out
}

/// RGBA → 1bpp 逐行 MSB-first 压缩 AND 掩码（CreateIconIndirect 的 hbmMask）。
///
/// 行跨度 = ((size + 15) / 16) * 2 字节（DWORD 对齐）。位语义：**1 = 透明**，
/// 仅在 alpha==0 的像素置 1（掩码只负责全透明区域，半透明交给 alpha 混合；
/// 全 0 掩码 + 32bpp alpha 是规范组合，v0.3.3 旧路径把掩码错写成逐字节扩展，
/// 是 CR1/真机色差的同源根因之一）。纯函数，可跨平台单测。
pub fn and_mask_packed_from_alpha(rgba: &[u8], size: u32) -> Vec<u8> {
    let size = size as usize;
    let stride = size.div_ceil(16) * 2;
    let mut mask = vec![0u8; stride * size];
    for y in 0..size {
        for x in 0..size {
            if rgba[(y * size + x) * 4 + 3] == 0 {
                mask[y * stride + x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    mask
}

/// RGBA 缓冲 → HICON（v0.3.4 #1 根因修复的核心路径）。
///
/// 根因：tray-icon 0.26.1 的 `Icon::from_rgba` 走 `CreateIcon`，把非预乘
/// 32bpp 数据当 DDB 颜色平面使用，且 AND 掩码按「每像素一字节」错位填充
/// （规范要求 1bpp 压缩位图）。Win32 语义下 DDB 的 alpha 通道不被尊重，
/// 导致托盘对 32bpp 图标的 alpha 解释错乱——透明像素污染 RGB、自定义
/// 背景色被曲解发灰发暗（真机 #1 与质检 CR1 同源）。
///
/// 修复：自建规范 HICON——`CreateDIBSection`（32bpp top-down DIB，Win32
/// 唯一尊重 alpha 的路径）写入预乘 BGRA；`CreateBitmap` 建 1bpp 掩码；
/// `CreateIconIndirect` 组装。HICON 所有权经 `tray_icon::Icon::from_handle`
/// 移交给 tray-icon（其 Drop 会 DestroyIcon），本函数返回后两个临时位图
/// 立即 DeleteObject，不泄漏。
///
/// v0.3.8（B-1）：失败返回具体 GDI 阶段（DC / DIB / IconIndirect），
/// 供启动路径的 bootlog 与出厂默认回退决策使用。
pub fn rgba_to_hicon(rgba: &[u8], size: u32) -> Result<isize, FrameError> {
    let size_i = size as i32;
    let n = (size * size) as usize;
    if rgba.len() != n * 4 {
        return Err(FrameError::BadBufferSize {
            expected: n * 4,
            actual: rgba.len(),
        });
    }
    unsafe {
        let hdc = CreateCompatibleDC(None);
        if hdc.is_invalid() {
            return Err(FrameError::DcCreateFailed);
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
        let Ok(hcolor) = CreateDIBSection(Some(hdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
        else {
            let _ = DeleteDC(hdc);
            return Err(FrameError::DibCreateFailed);
        };
        if bits.is_null() {
            let _ = DeleteObject(HGDIOBJ(hcolor.0));
            let _ = DeleteDC(hdc);
            return Err(FrameError::DibCreateFailed);
        }
        let bgra = premultiplied_bgra_from_rgba(rgba);
        std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits as *mut u8, n * 4);
        let mask = and_mask_packed_from_alpha(rgba, size);
        // CreateBitmap 单平面 1bpp：hbmMask 规范形态。
        let hmask = CreateBitmap(size_i, size_i, 1, 1, Some(mask.as_ptr().cast()));
        let hmask = if hmask.is_invalid() {
            HBITMAP::default()
        } else {
            hmask
        };
        let info = ICONINFO {
            fIcon: windows::core::BOOL(1),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: hmask,
            hbmColor: hcolor,
        };
        let hicon = CreateIconIndirect(&info);
        // 临时位图的所有权在 CreateIconIndirect 复制后即可释放。
        if !hmask.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(hmask.0));
        }
        let _ = DeleteObject(HGDIOBJ(hcolor.0));
        let _ = DeleteDC(hdc);
        match hicon {
            Ok(h) if !h.is_invalid() => Ok(h.0 as isize),
            _ => Err(FrameError::IconCreateFailed),
        }
    }
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
        let mut cache = IconCache::new(
            size,
            true,
            IconShape::Rounded,
            bg,
            IconFontColor::Auto,
            "Segoe UI".to_string(),
            None,
            100,
            100,
        );
        let rgba = cache
            .map_for_test(IconColor::Default, "50", false)
            .expect("normal-level bitmap renders on demand")
            .to_vec();
        let idx = ((2 * size + size / 2) * 4) as usize;
        let (er, eg, eb) = bg.rgb();
        assert_eq!(&rgba[idx..idx + 3], &[er, eg, eb]);
        assert_eq!(rgba[idx + 3], 255);

        // 对照：透明形状同位置无背景。
        let mut transparent = IconCache::new(
            size,
            true,
            IconShape::Transparent,
            bg,
            IconFontColor::Auto,
            "Segoe UI".to_string(),
            None,
            100,
            100,
        );
        let rgba_t = transparent
            .map_for_test(IconColor::Default, "50", false)
            .expect("transparent bitmap renders on demand")
            .to_vec();
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
                100,
                100,
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
            100,
            100,
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

    /// v0.3.5 #4 验收：透明背景 + 低电 → 闪烁帧走「文本帧路径」——字色
    /// 跳变之外，文本透明度被脉冲（255 → 115）；非透明形状维持背景闪烁、
    /// 文本 alpha 不动。
    #[test]
    fn transparent_blink_frames_pulse_text_alpha() {
        let size = 32u32;
        let mut cache = IconCache::new(
            size,
            true,
            IconShape::Transparent,
            IconBackground::Dark,
            IconFontColor::Auto,
            "Segoe UI".to_string(),
            None,
            100,
            100,
        );
        let normal = cache
            .map_for_test(IconColor::Default, "50", false)
            .expect("normal frame renders on demand")
            .to_vec();
        let bright = cache
            .map_for_test(IconColor::Default, "50", true)
            .expect("bright frame renders on demand")
            .to_vec();
        assert_ne!(normal, bright, "blink frames must differ on transparent bg");
        // 实心字形像素：alpha 255 → pulse_alpha(255)（=115），颜色通道也变。
        let solid = normal
            .chunks_exact(4)
            .position(|px| px[3] == 255)
            .expect("solid glyph pixel expected");
        let (n, b) = (
            &normal[solid * 4..solid * 4 + 4],
            &bright[solid * 4..solid * 4 + 4],
        );
        assert_eq!(n[3], 255);
        assert_eq!(b[3], pulse_alpha(255, BLINK_TEXT_ALPHA_PULSE));
        assert_ne!(&n[..3], &b[..3], "text color must also jump");
        // 透明形状下背景区像素两帧都全透明（没有背景可闪）。
        let corner = 0usize;
        assert_eq!(normal[corner + 3], 0);
        assert_eq!(bright[corner + 3], 0);

        // 对照：非透明形状维持背景闪烁路径——背景像素 alpha 恒 255，
        // 文本像素 alpha 不脉冲，差异只在颜色通道。
        let mut cache2 = IconCache::new(
            size,
            true,
            IconShape::Rounded,
            IconBackground::Dark,
            IconFontColor::Auto,
            "Segoe UI".to_string(),
            None,
            100,
            100,
        );
        let normal2 = cache2
            .map_for_test(IconColor::Default, "50", false)
            .expect("normal frame renders on demand")
            .to_vec();
        let bright2 = cache2
            .map_for_test(IconColor::Default, "50", true)
            .expect("bright frame renders on demand")
            .to_vec();
        assert_ne!(normal2, bright2, "bg flash frames must differ");
        let idx = ((2 * size + size / 2) * 4) as usize; // 背景采样点（同 P1-1 回归）
        assert_eq!(normal2[idx + 3], 255);
        assert_eq!(bright2[idx + 3], 255);
        assert_ne!(
            &normal2[idx..idx + 3],
            &bright2[idx..idx + 3],
            "background color must flash"
        );
    }

    /// #4：分档模式 —— N 个分档色 + 灰档帧都能按需渲染（v0.3.15 懒渲染：
    /// 缓存启动为空，任何被请求的颜色档都现场渲染，不再有「fill 键集合」）。
    #[test]
    fn tier_frames_render_on_demand() {
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
        let mut cache = IconCache::new(
            16,
            true,
            IconShape::Rounded,
            IconBackground::Dark,
            IconFontColor::Auto,
            "Segoe UI".to_string(),
            Some(tiers),
            100,
            100,
        );
        assert_eq!(cache.cached_len_for_test(), 0, "懒渲染：启动零预渲染帧");
        for i in 0..3u8 {
            assert!(
                cache
                    .map_for_test(IconColor::Tier(i), "50", false)
                    .is_some(),
                "tier {i} frame must render on demand"
            );
        }
        assert!(cache.map_for_test(IconColor::Gray, "50", false).is_some());
        assert_eq!(cache.cached_len_for_test(), 4, "只缓存实际请求过的帧");
    }

    /// v0.3.8（D-64）根因回归测试：v0.3.5 D-56 迁移把「分档关 + 字色
    /// auto」的旧配置迁成分档开启后，启动首帧查询必须与 pump 的
    /// `percent == None` 分支同决策（Gray 档）。v0.3.4 以来的启动代码
    /// 固定查 `IconColor::Default`——分档模式下该键不在 fill 的键集合里，
    /// 启动即「default frame missing」退出（真机日志 stage: icon-cache /
    /// exit code=1）。v0.3.15 懒渲染后该类故障结构上不再可能（任何被
    /// 请求的帧现场渲染），本测试保留「启动取色与 pump 同决策」的锁定。
    #[test]
    fn tier_mode_startup_frame_resolves_after_root_fix() {
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
        // 模拟 v0.3.5 D-56 迁移后的用户配置视图（分档开启）。
        let mut cache = IconCache::new(
            16,
            true,
            IconShape::Rounded,
            IconBackground::Dark,
            IconFontColor::Auto,
            "Segoe UI".to_string(),
            Some(tiers),
            100,
            100,
        );
        // 修复后（绿）：启动取色与 pump 对齐（Gray 档），首帧渲染成功。
        let color = startup_frame_color(true);
        assert_eq!(color, IconColor::Gray);
        assert!(cache.icon_for_checked(color, "?", false).is_ok());
        // 对照：非分档模式启动仍走 Default 档（v0.3.4 行为不变）。
        assert_eq!(startup_frame_color(false), IconColor::Default);
    }

    /// v0.3.8（B-1）回退验收：分档开启的用户配置首帧查询失败后，用出厂
    /// 默认渲染配置重建缓存（`cache_from_config(Config::default())`），
    /// 默认帧必须生成成功——「该配置值 → 默认帧生成成功（回退后）」。
    /// （v0.3.15 懒渲染后缺帧类失败不再存在，保留 GDI 级失败回退路径的
    /// 验收：出厂默认配置的首帧必须生成成功。）
    #[test]
    fn factory_default_cache_generates_first_frame_after_fallback() {
        // 出厂默认渲染配置重试（main 启动回退同一路径）。
        let factory_cfg = Config::default();
        let mut factory = cache_from_config(&factory_cfg, 16, true);
        assert!(factory
            .icon_for_checked(
                startup_frame_color(factory_cfg.icon_font_tiers_enabled),
                "?",
                false
            )
            .is_ok());
    }

    /// v0.3.4 #1 验收：纯红 (255,0,0) 自定义背景，常态渲染位图背景像素
    /// 主通道必须是 R——同时拦截「字节序颠倒（设红显蓝）」与「alpha 污染
    /// （发灰发暗）」两类回归；并验证预乘 BGRA 输出字节序正确。
    #[test]
    fn pure_red_custom_bg_pixel_is_red() {
        let size = 32u32;
        let bg = IconBackground::Custom(RgbColor { r: 255, g: 0, b: 0 });
        let mut cache = IconCache::new(
            size,
            true,
            IconShape::Rounded,
            bg,
            IconFontColor::Auto,
            "Segoe UI".to_string(),
            None,
            100,
            100,
        );
        let rgba = cache
            .map_for_test(IconColor::Default, "50", false)
            .expect("bitmap renders on demand")
            .to_vec();
        let idx = ((2 * size + size / 2) * 4) as usize;
        assert_eq!(
            &rgba[idx..idx + 3],
            &[255, 0, 0],
            "bg pixel must be pure red (RGB order)"
        );
        assert_eq!(rgba[idx + 3], 255, "bg pixel must be opaque");
        let bgra = premultiplied_bgra_from_rgba(&rgba);
        // 预乘 BGRA：B=0, G=0, R=255, A=255（字节序颠倒会变成 [255,0,0,255]）。
        assert_eq!(&bgra[idx..idx + 4], &[0, 0, 255, 255]);
    }

    /// v0.3.4 #1：预乘 + 字节序换算的数学正确性。
    #[test]
    fn premultiplied_bgra_math_and_byte_order() {
        // 纯红不透明：BGRA = [0,0,255,255]。
        assert_eq!(
            premultiplied_bgra_from_rgba(&[255, 0, 0, 255]),
            &[0, 0, 255, 255]
        );
        // 纯蓝不透明：BGRA = [255,0,0,255]。
        assert_eq!(
            premultiplied_bgra_from_rgba(&[0, 0, 255, 255]),
            &[255, 0, 0, 255]
        );
        // 半透明：B=(64*128+127)/255=32, G=(128*128+127)/255=64, R=(255*128+127)/255=128。
        assert_eq!(
            premultiplied_bgra_from_rgba(&[255, 128, 64, 128]),
            &[32, 64, 128, 128]
        );
        // 全透明像素 RGB 归零（alpha 污染回归的护栏）。
        assert_eq!(premultiplied_bgra_from_rgba(&[255, 0, 0, 0]), &[0, 0, 0, 0]);
    }

    /// v0.3.4 #1：1bpp MSB-first 压缩掩码。2x2，alpha = [255,0,0,255]：
    /// 行 0 字节 0 = 0b0100_0000（第 1 像素透明），行 1 字节 = 0b1000_0000。
    #[test]
    fn and_mask_packed_msb_first() {
        let rgba = [
            255u8, 0, 0, 255, // (0,0) 不透明
            0, 0, 0, 0, // (1,0) 透明
            0, 0, 0, 0, // (0,1) 透明
            255, 0, 0, 255, // (1,1) 不透明
        ];
        let mask = and_mask_packed_from_alpha(&rgba, 2);
        // 行跨度 = ((2+15)/16)*2 = 2 字节；总长 4。
        assert_eq!(mask.len(), 4);
        assert_eq!(mask, &[0x40, 0x00, 0x80, 0x00]);
        // 全不透明 → 掩码全 0（v0.3.3 旧路径逐字节掩码的回归护栏）。
        let solid = vec![255u8; 4 * 4];
        assert!(and_mask_packed_from_alpha(&solid, 2)
            .iter()
            .all(|b| *b == 0));
    }

    /// v0.3.18（R4 BF-1）：缩字机制单调性——文本大小档位越高，渲染墨迹
    /// 高度不得变小（旧比例缩字在整数截断下，size=24 时 ts=160 反矮于
    /// ts=140）。逐档递减修复后对全部位图档位 × 全部文本档位验证。
    #[test]
    fn shrink_keeps_glyph_height_monotonic() {
        for size in [16u32, 24, 32, 48] {
            let mut prev_h = 0i32;
            for ts in (40u8..=160).step_by(10) {
                let rgba = render_text_bitmap(
                    size,
                    "83",
                    (0xFF, 0xFF, 0xFF),
                    None,
                    IconShape::Transparent,
                    "Segoe UI",
                    ts,
                    100,
                );
                let mut y0 = i32::MAX;
                let mut y1 = i32::MIN;
                for (i, px) in rgba.chunks_exact(4).enumerate() {
                    if px[3] >= 0x10 {
                        let py = (i / size as usize) as i32;
                        y0 = y0.min(py);
                        y1 = y1.max(py);
                    }
                }
                assert!(y0 <= y1, "size={size} ts={ts}: no ink rendered");
                let h = y1 - y0 + 1;
                assert!(
                    h >= prev_h,
                    "size={size}: ts={ts} ink height {h} < previous {prev_h} (non-monotonic)"
                );
                prev_h = h;
            }
        }
    }

    /// v0.3.18（U-4 回归）：粗字重字体在最高档位下，墨迹必须完整落在
    /// 背景块内（图2：Arial Black @160% 底部裁切、未居中）。探针循环按
    /// 真实墨迹判定，GDI 字体回退（无 Arial Black 的环境）同样被约束。
    #[test]
    fn heavy_font_at_max_scale_stays_within_block() {
        let size = 16u32;
        let bs = 80u8;
        let block = size as i32 * bs as i32 / 100; // 12
        let lo = (size as i32 - block) / 2; // 2
        let hi = lo + block - 1; // 13
        let with_text = render_text_bitmap(
            size,
            "83",
            (0xFF, 0xFF, 0xFF),
            Some((0x80, 0, 0)),
            IconShape::Square,
            "Arial Black",
            160,
            bs,
        );
        let bg_only = render_text_bitmap(
            size,
            "",
            (0xFF, 0xFF, 0xFF),
            Some((0x80, 0, 0)),
            IconShape::Square,
            "Arial Black",
            160,
            bs,
        );
        let mut ink = 0usize;
        for i in 0..(size * size) as usize {
            if with_text[i * 4..i * 4 + 4] != bg_only[i * 4..i * 4 + 4] {
                let (py, px) = ((i / size as usize) as i32, (i % size as usize) as i32);
                assert!(
                    (lo..=hi).contains(&px) && (lo..=hi).contains(&py),
                    "ink outside block at ({px},{py}); block = [{lo},{hi}]"
                );
                ink += 1;
            }
        }
        assert!(ink > 0, "no ink rendered (text missing entirely)");
    }

    /// v0.3.4 #4 / v0.3.5 #8（D-57）验收：缩放矩阵（文本 40/100/160 × 背景
    /// 覆盖度 20/60/100 × 圆/方）下，文字墨迹不超出背景块、背景块不超出
    /// 位图边界（100% = 满出血贴边）。
    #[test]
    fn scale_matrix_keeps_text_within_block_and_bitmap() {
        let size = 32u32;
        for text_scale in [40u8, 100, 160] {
            for bg_scale in [20u8, 60, 100] {
                for shape in [IconShape::Rounded, IconShape::Square] {
                    for text in ["50", "满"] {
                        let bg = (0x0A, 0x6C, 0xFF);
                        let with_text = render_text_bitmap(
                            size,
                            text,
                            (255, 255, 255),
                            Some(bg),
                            shape,
                            "Segoe UI",
                            text_scale,
                            bg_scale,
                        );
                        let bg_only = render_text_bitmap(
                            size,
                            "",
                            (255, 255, 255),
                            Some(bg),
                            shape,
                            "Segoe UI",
                            text_scale,
                            bg_scale,
                        );
                        // 文字墨迹 = 与纯背景帧的差异像素。
                        let mut ink = Vec::new();
                        for i in 0..(size * size) as usize {
                            if with_text[i * 4..i * 4 + 4] != bg_only[i * 4..i * 4 + 4] {
                                ink.push(i);
                            }
                        }
                        assert!(!ink.is_empty(), "no ink at ts={text_scale} bs={bg_scale}");
                        // 背景块边界（与渲染几何同式）。
                        let block = size as i32 * bg_scale as i32 / 100;
                        let lo = (size as i32 - block) / 2;
                        let hi = lo + block - 1;
                        for i in &ink {
                            let (py, px) =
                                ((*i / size as usize) as i32, (*i % size as usize) as i32);
                            assert!(
                                px >= lo && px <= hi && py >= lo && py <= hi,
                                "ink at ({px},{py}) outside block [{lo},{hi}]                                  ts={text_scale} bs={bg_scale} shape={shape:?} text={text}"
                            );
                        }
                        // 背景块不超出位图：所有非透明像素都在位图内（天然成立），
                        // 再验边界值合法。
                        assert!(lo >= 0 && hi < size as i32);
                    }
                }
            }
        }
    }

    /// v0.3.15（P1-2）：渲染输入变化（此处以 DPI 尺寸变化为代表）→
    /// rebuild 清空缓存；输入未变时 rebuild 不动缓存（零开销判等）。
    #[test]
    fn rebuild_clears_cached_frames_on_input_change() {
        let mut cache = IconCache::new(
            16,
            true,
            IconShape::Rounded,
            IconBackground::Dark,
            IconFontColor::Auto,
            "Segoe UI".to_string(),
            None,
            100,
            100,
        );
        let _ = cache.map_for_test(IconColor::Default, "50", false);
        assert_eq!(cache.cached_len_for_test(), 1);
        // 相同输入 rebuild：缓存保留（旧 fill 行为是全清重渲，现为零开销）。
        cache.rebuild(
            16,
            true,
            IconShape::Rounded,
            IconBackground::Dark,
            IconFontColor::Auto,
            "Segoe UI".to_string(),
            None,
            100,
            100,
        );
        assert_eq!(cache.cached_len_for_test(), 1, "输入未变不得清缓存");
        // DPI 变化（16 → 32px）：旧帧尺寸全失效，必须清空。
        cache.rebuild(
            32,
            true,
            IconShape::Rounded,
            IconBackground::Dark,
            IconFontColor::Auto,
            "Segoe UI".to_string(),
            None,
            100,
            100,
        );
        assert_eq!(cache.cached_len_for_test(), 0, "DPI 变更必须清缓存");
        // 清空后按新尺寸懒渲染。
        let frame = cache
            .map_for_test(IconColor::Default, "50", false)
            .expect("re-render after DPI change");
        assert_eq!(frame.len(), (32 * 32 * 4) as usize, "新帧须为新尺寸");
    }

    /// v0.3.15（P1-2）：闪烁两相位帧跨调用命中缓存——反复交替取
    /// 常态/提亮帧，缓存帧数恒为 2（每 500ms 相位切换不重渲染）。
    #[test]
    fn blink_two_phase_frames_cached_across_calls() {
        let mut cache = IconCache::new(
            16,
            true,
            IconShape::Rounded,
            IconBackground::Dark,
            IconFontColor::Auto,
            "Segoe UI".to_string(),
            None,
            100,
            100,
        );
        let normal_first = cache
            .map_for_test(IconColor::Red, "15", false)
            .expect("normal frame")
            .to_vec();
        for _ in 0..50 {
            let n = cache
                .map_for_test(IconColor::Red, "15", false)
                .expect("normal frame cached");
            assert_eq!(n, normal_first.as_slice(), "命中必须返回同一帧");
            let _ = cache
                .map_for_test(IconColor::Red, "15", true)
                .expect("bright frame cached");
        }
        assert_eq!(
            cache.cached_len_for_test(),
            2,
            "两相位各一帧，50 轮交替无淘汰无重渲"
        );
    }
}
