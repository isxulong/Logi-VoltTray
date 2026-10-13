//! 配置 schema（v0.3.0 设置体系 M9-M13）+ 容错解析 + mtime 热重载。
//!
//! 配置文件：`%APPDATA%\LogiVoltTray\config.toml`（扁平键值，无 section）。
//! 解析器为扁平 TOML 子集（bool / 整数 / 字符串），见 docs/decisions.md D-07。
//! 容错规则：未知键 / 非法值 → 回退默认 + 诊断警告；越界值 → **钳制到边界 +
//! 警告**（M10：不再重置默认）；绝不 panic、绝不拒绝启动。
//!
//! 键位一览（v0.3.5，共 16 键）：
//! | 键 | 类型 | 默认 | 取值范围 / 说明 |
//! | --- | --- | --- | --- |
//! | `autostart` | bool | `false` | 开机自启（写入启动快捷方式） |
//! | `poll_interval_secs` | int | `600` | 档位 `{60, 300, 600, 900, 1800}`（D-45；越界钳制 / 非档位吸附最近档 + 警告） |
//! | `icon_style` | str | `"auto"` | `auto`/`light`/`dark`（整体风格提示键，v0.3.0 不参与渲染） |
//! | `notifications_enabled` | bool | `true` | 低电量通知开关 |
//! | `notification_silent` | bool | `false` | 通知静默（无提示音） |
//! | `icon_shape` | str | `"rounded"` | `rounded` / `square` / `transparent`（M9） |
//! | `icon_background` | str | `"dark"` | `light` / `dark` / `"#RRGGBB"`（预设或自定义；`transparent` 迁移到 `icon_shape`，M9） |
//! | `icon_font_color` | str | `"auto"` | `"auto"` 或 `"#RRGGBB"`（M9；分档模式启用时被忽略） |
//! | `icon_font_family` | str | `"Segoe UI"` | 系统字体名（渲染时缺失则 GDI 静默回退） |
//! | `low_threshold` | int | `20` | `5`–`50`，低电提醒阈值（M12 单阈值，越界钳制） |
//! | `blink_enabled` | bool | `true` | 低电闪烁提醒（M11，可关） |
//! | `icon_font_tiers_enabled` | bool | `true` | 按电量分档着色（v0.3.5 #5 默认开：字体颜色「自动(按电量档位)」即分档；旧配置「勾选关 + auto」迁移为开，D-56） |
//! | `icon_font_tier_thresholds` | int 数组 | `[20, 50]` | N-1 个边界（N 档 2..=5）；阈值含该值归上一档；1–99 严格递增（越界钳制 / 递进修复 + 警告） |
//! | `icon_font_tier_colors` | str 数组 | `["#E81123", "#FFD21F", "#00CC6A"]` | N 个分档色（缺失补默认色、多余截断 + 警告） |
//! | `icon_text_scale` | int | `100` | `40`–`160`，图标文本大小（%，D-48/D-57 范围拓展，越界钳制） |
//! | `icon_bg_scale` | int | `100` | `20`–`100`，图标背景覆盖度（%，D-57；100% = 满出血零边距，几何上限） |
//!
//! 废弃键（保留迁移语义）：
//! - `low_threshold_warn`：迁移为 `low_threshold`（钳 5..=50）+ 警告；
//! - `low_threshold_critical`：忽略 + 警告（M12 单阈值取代双阈值）。

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::time::SystemTime;

/// 轮询间隔默认值（D-45：600 秒——电量变化约 1%–2%/小时，300s 偏勤快；
/// 低电提醒及时性在阈值 ≥5% 时 10 分钟级延迟完全可接受）。
pub const DEFAULT_POLL_SECS: u64 = 600;
pub const POLL_MIN_SECS: u64 = 60;
pub const POLL_MAX_SECS: u64 = 1800;
/// 轮询档位（D-45）：设置窗口与配置解析共用；15/30/120 秒三档已移除
/// （过短档位与零耗电理念相悖）。
pub const POLL_TIERS_SECS: [u64; 5] = [60, 300, 600, 900, 1800];
pub const DEFAULT_LOW_THRESHOLD: u8 = 20;
pub const LOW_THRESHOLD_MIN: u8 = 5;
pub const LOW_THRESHOLD_MAX: u8 = 50;
/// 图标文本大小下限（%，D-48；D-57 由 60 拓展到 40）。
pub const ICON_TEXT_SCALE_MIN: u8 = 40;
/// 图标文本大小上限（%，D-48；D-57 由 140 拓展到 160）。
pub const ICON_TEXT_SCALE_MAX: u8 = 160;
/// 图标背景覆盖度下限（%，D-57 由 60 拓展到 20）。
pub const ICON_BG_SCALE_MIN: u8 = 20;
/// 图标背景覆盖度上限（%，D-48/D-57）：100% = 满出血零边距，已触及位图
/// 边界，更大会溢出，故不设 >100%。
pub const ICON_BG_SCALE_MAX: u8 = 100;
/// 字体颜色与背景对比度下限（M9：<3.0 仅警告，不阻断）。
pub const CONTRAST_FLOOR: f64 = 3.0;

// ---------- 分档字体颜色（v0.3.3 #4） ----------

/// 分档数量下限（N 档 = N-1 个阈值 + N 个颜色）。
pub const FONT_TIER_MIN_TIERS: usize = 2;
/// 分档数量上限（对应阈值 ≤ 4 个）。
pub const FONT_TIER_MAX_TIERS: usize = 5;
/// 阈值取值范围 1..=99（电量 0%/100% 不允许作为边界）。
pub const FONT_TIER_THRESHOLD_MIN: u8 = 1;
pub const FONT_TIER_THRESHOLD_MAX: u8 = 99;
/// 默认分档：0..=20 / 21..=50 / 51..=100 三档（红/黄/绿）。
pub const DEFAULT_FONT_TIER_THRESHOLDS: [u8; 2] = [20, 50];
/// 与默认阈值配套的 3 个分档色。
pub const DEFAULT_FONT_TIER_COLORS: [RgbColor; 3] = [
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
/// 颜色不足时依序补位的调色板（红/橙/黄/绿/蓝）。
pub const TIER_PAD_PALETTE: [RgbColor; 5] = [
    RgbColor {
        r: 0xE8,
        g: 0x11,
        b: 0x23,
    },
    RgbColor {
        r: 0xFF,
        g: 0x8C,
        b: 0x00,
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
    RgbColor {
        r: 0x00,
        g: 0x78,
        b: 0xD7,
    },
];

/// 电量百分比的档位序号（0 基）：阈值含该值归上一档。
/// 例：thresholds = [20, 50] 时 0..=20 → 0，21..=50 → 1，51..=100 → 2。
pub fn font_tier_for_percent(thresholds: &[u8], percent: u8) -> usize {
    thresholds.iter().filter(|&&t| percent > t).count()
}

/// 分档数据规范化：钳制阈值到 1..=99、修复非严格递增、数量对齐
/// （阈值截断到 MAX-1；颜色不足补调色板、多余截断），返回修复警告。
/// 对已合规数据是幂等的（不产生警告）。
pub fn normalize_font_tiers(
    thresholds: Vec<u8>,
    colors: Vec<RgbColor>,
) -> (Vec<u8>, Vec<RgbColor>, Vec<ConfigWarning>) {
    let mut warnings = Vec::new();
    let mut warn = |reason: String| {
        warnings.push(ConfigWarning {
            key: "icon_font_tier_thresholds".into(),
            reason,
        })
    };

    let tier_count = thresholds.len() + 1;
    if !(FONT_TIER_MIN_TIERS..=FONT_TIER_MAX_TIERS).contains(&tier_count) {
        warn(format!(
            "tier count {} out of range {FONT_TIER_MIN_TIERS}..={FONT_TIER_MAX_TIERS}; reset to default 3",
            tier_count
        ));
        return (
            DEFAULT_FONT_TIER_THRESHOLDS.to_vec(),
            DEFAULT_FONT_TIER_COLORS.to_vec(),
            warnings,
        );
    }

    // 钳制 + 严格递增修复：nxt = max(t, prev + 1)；超出 99 则丢弃余下阈值。
    let mut fixed: Vec<u8> = Vec::with_capacity(thresholds.len());
    let mut prev: u8 = 0;
    for &t in &thresholds {
        let clamped = t.clamp(FONT_TIER_THRESHOLD_MIN, FONT_TIER_THRESHOLD_MAX);
        if clamped != t {
            warn(format!(
                "threshold {t} out of range {FONT_TIER_THRESHOLD_MIN}..={FONT_TIER_THRESHOLD_MAX}; clamped to {clamped}"
            ));
        }
        let nxt = clamped.max(prev + 1);
        if nxt > FONT_TIER_THRESHOLD_MAX {
            warn(format!(
                "thresholds not strictly increasing beyond {FONT_TIER_THRESHOLD_MAX}; trailing thresholds dropped"
            ));
            break;
        }
        if nxt > clamped {
            warn(format!(
                "thresholds not strictly increasing; {clamped} raised to {nxt}"
            ));
        }
        fixed.push(nxt);
        prev = nxt;
    }

    let tier_count = fixed.len() + 1;
    if fixed.len() + 1 < FONT_TIER_MIN_TIERS {
        // 全部阈值被丢弃（理论上仅在输入全 >99 且无法递进时发生）。
        warn("no valid thresholds left; reset to default 3".into());
        return (
            DEFAULT_FONT_TIER_THRESHOLDS.to_vec(),
            DEFAULT_FONT_TIER_COLORS.to_vec(),
            warnings,
        );
    }

    // 颜色数量对齐：不足补调色板（跳过已有色）、多余截断。
    let mut col = colors;
    let mut pad_i = 0usize;
    while col.len() < tier_count {
        let candidate = TIER_PAD_PALETTE[pad_i % TIER_PAD_PALETTE.len()];
        if !col.contains(&candidate) {
            warnings.push(ConfigWarning {
                key: "icon_font_tier_colors".into(),
                reason: format!("too few colors; padded with {}", candidate.to_hex()),
            });
            col.push(candidate);
        }
        pad_i += 1;
        if pad_i > TIER_PAD_PALETTE.len() * 2 {
            // 调色板被自定义色占满时退回顺序补位。
            warnings.push(ConfigWarning {
                key: "icon_font_tier_colors".into(),
                reason: format!("padded with {}", candidate.to_hex()),
            });
            col.push(candidate);
        }
    }
    if col.len() > tier_count {
        warnings.push(ConfigWarning {
            key: "icon_font_tier_colors".into(),
            reason: format!("too many colors; truncated to {tier_count}"),
        });
        col.truncate(tier_count);
    }
    (fixed, col, warnings)
}

/// 图标配色策略（v0.2.0 遗留风格键；v0.3.0 不参与渲染决策）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IconStyle {
    Auto,
    Light,
    Dark,
}

impl IconStyle {
    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }
}

/// 托盘图标形状（M9：圆角 / 直角 / 透明无背景）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IconShape {
    /// 圆形 / 圆角底（默认）。
    Rounded,
    /// 直角方块底。
    Square,
    /// 透明无背景，仅渲染数字。
    Transparent,
}

impl IconShape {
    pub fn name(self) -> &'static str {
        match self {
            Self::Rounded => "rounded",
            Self::Square => "square",
            Self::Transparent => "transparent",
        }
    }
}

/// 托盘图标背景色（M9：预设或自定义 HEX；透明语义移交给 `IconShape`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum IconBackground {
    /// 浅色底（近似白），配深色数字。
    Light,
    /// 深色底（近似黑），配浅色数字——默认。
    Dark,
    /// 自定义底色（#RRGGBB）。
    Custom(RgbColor),
}

impl IconBackground {
    pub fn name(&self) -> String {
        match self {
            Self::Light => "light".to_string(),
            Self::Dark => "dark".to_string(),
            Self::Custom(c) => c.to_hex(),
        }
    }

    /// 背景填充 RGB（渲染层在 `IconShape::Transparent` 时不使用）。
    pub fn rgb(self) -> (u8, u8, u8) {
        match self {
            Self::Light => (0xF2, 0xF2, 0xF2),
            Self::Dark => (0x1F, 0x1F, 0x1F),
            Self::Custom(c) => (c.r, c.g, c.b),
        }
    }
}

/// RGB 颜色（配置解析时已校验）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct RgbColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl RgbColor {
    /// 解析 `#RRGGBB`（大小写不敏感，必须 6 位十六进制）。
    pub fn parse_hex(s: &str) -> Option<Self> {
        let hex = s.strip_prefix('#')?;
        if hex.len() != 6 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        let v = u32::from_str_radix(hex, 16).ok()?;
        Some(RgbColor {
            r: (v >> 16) as u8,
            g: (v >> 8) as u8,
            b: v as u8,
        })
    }

    pub fn to_hex(self) -> String {
        format!("#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
    }
}

/// 电量字体颜色设置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum IconFontColor {
    /// 自动：按电量档位着色（正常/警告/严重），正常档颜色依背景取高对比。
    Auto,
    /// 固定色：覆盖正常/警告/严重档，未知(?)与灰显(--)保持灰。
    Fixed(RgbColor),
}

impl IconFontColor {
    pub fn name(&self) -> String {
        match self {
            Self::Auto => "auto".to_string(),
            Self::Fixed(c) => c.to_hex(),
        }
    }
}

// ---------- 对比度 / 提亮纯函数（M9 对比度警告、M11 闪烁提亮用） ----------

/// WCAG 相对亮度。
fn relative_luminance((r, g, b): (u8, u8, u8)) -> f64 {
    let ch = |v: u8| {
        let s = v as f64 / 255.0;
        if s <= 0.039_28 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * ch(r) + 0.7152 * ch(g) + 0.0722 * ch(b)
}

/// WCAG 对比度（1..=21）。
pub fn contrast_ratio(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
    let la = relative_luminance(a);
    let lb = relative_luminance(b);
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// 字色对背景是否可读（M9：对比度 ≥ 3.0；不足仅警告不阻断）。
pub fn contrast_ok(font: (u8, u8, u8), bg: (u8, u8, u8)) -> bool {
    contrast_ratio(font, bg) >= CONTRAST_FLOOR
}

/// 轮询间隔归一化（D-45）：先钳制到 [60, 1800]，再吸附到最近档位；
/// 距离相同取更低档（更省电）。旧配置中被移除的 15/30/120 秒档由此归位
/// （15/30 → 60，120 → 60）。设置窗口只写档位值，此函数保证 TOML 手改
/// 任意值也落在档位上（设置窗口下拉才能正确回显）。
pub fn normalize_poll_secs(v: u64) -> u64 {
    let clamped = v.clamp(POLL_MIN_SECS, POLL_MAX_SECS);
    POLL_TIERS_SECS
        .iter()
        .copied()
        .min_by_key(|t| (t.abs_diff(clamped), *t))
        .unwrap_or(DEFAULT_POLL_SECS)
}

/// 提亮（v0.3.4 #3 根因修复：向白色线性插值）。
///
/// 旧实现是乘法提亮 `v × (1 + factor)`：饱和通道封顶不动——纯红背景
/// (255,0,0) 提亮后两帧位图完全相同；暗通道变化量 ~12/255 肉眼不可见。
/// 这正是真机「低电闪烁未生效」的根因。改为向白插值
/// `v + (255 − v) × factor` 后，任何非纯白色都有可感知的亮度跳变。
pub fn brighten((r, g, b): (u8, u8, u8), factor: f64) -> (u8, u8, u8) {
    let f = |v: u8| -> u8 { ((v as f64 + (255.0 - v as f64) * factor) + 0.5) as u8 };
    (f(r), f(g), f(b))
}

/// 闪烁「提亮帧」取色（v0.3.4 #3）：一般颜色向白插值（`brighten`）；
/// 近白色（对纯白对比度 ≤ 1.2，含纯白字 / 浅色底）反向向深灰 (0x1B,0x1B,0x1B)
/// 插值——否则纯白文本的透明底图标闪烁两帧仍完全相同（可见性盲区）。
pub fn flash_rgb(c: (u8, u8, u8), factor: f64) -> (u8, u8, u8) {
    const DARK: (u8, u8, u8) = (0x1B, 0x1B, 0x1B);
    if contrast_ratio(c, (0xFF, 0xFF, 0xFF)) <= 1.2 {
        let f =
            |v: u8| -> u8 { ((v as f64 * (1.0 - factor)) + (DARK.0 as f64 * factor) + 0.5) as u8 };
        (f(c.0), f(c.1), f(c.2))
    } else {
        brighten(c, factor)
    }
}

// ---------- Win32 COLORREF 转换（v0.3.5 #1，D-52） ----------

/// RGB → Win32 COLORREF（0x00BBGGRR）。
///
/// **全仓唯一实现**：取色对话框 / HEX 输入 / 分档色点等所有与 Win32 交换
/// 颜色的入口必须用本函数对转换，禁止第二份实现。v0.3.4 的 P1 缺陷就是把
/// COLORREF 直接按 `#{:06X}` 序列化（BGR 当 RGB），纯红被写成 #0000FF。
pub fn rgb_to_colorref(c: RgbColor) -> u32 {
    c.r as u32 | ((c.g as u32) << 8) | ((c.b as u32) << 16)
}

/// Win32 COLORREF（0x00BBGGRR）→ RGB（忽略高 8 位）。
/// 与 `rgb_to_colorref` 互为逆运算，同样全仓唯一。
pub fn colorref_to_rgb(c: u32) -> RgbColor {
    RgbColor {
        r: (c & 0xFF) as u8,
        g: ((c >> 8) & 0xFF) as u8,
        b: ((c >> 16) & 0xFF) as u8,
    }
}

/// 透明背景下低电闪烁的文本透明度脉冲系数（v0.3.5 #4，D-55）：
/// 提亮帧文本 alpha × (1 − 0.55) ≈ 45%，与颜色跳变叠加保证肉眼可见。
pub const BLINK_TEXT_ALPHA_PULSE: f64 = 0.55;

/// 透明度脉冲：alpha × (1 − factor)（四舍五入）。`factor` 取
/// `BLINK_TEXT_ALPHA_PULSE`；普通帧不调用本函数（alpha 原样）。
pub fn pulse_alpha(alpha: u8, factor: f64) -> u8 {
    (alpha as f64 * (1.0 - factor) + 0.5) as u8
}

/// 配置（v0.3.5：16 键 + 废弃迁移）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Config {
    /// 开机自启（HKCU Run 单值）。
    pub autostart: bool,
    /// 轮询间隔（秒），默认 600（D-45），解析归一到档位
    /// {60, 300, 600, 900, 1800}（越界钳制 / 非档位吸附最近档 + 警告）。
    pub poll_interval_secs: u64,
    /// 整体风格提示键（v0.2.0 遗留，v0.3.0 不参与渲染）。
    pub icon_style: IconStyle,
    /// 通知总开关。
    pub notifications_enabled: bool,
    /// 静默通知（无声音）。
    pub notification_silent: bool,
    /// 托盘图标形状：圆角 / 直角 / 透明，默认圆角（M9）。
    pub icon_shape: IconShape,
    /// 托盘图标背景色：light / dark / #RRGGBB，默认 dark（M9）。
    pub icon_background: IconBackground,
    /// 电量字体颜色：auto（按电量档位）或 "#RRGGBB"，默认 auto（M9）。
    pub icon_font_color: IconFontColor,
    /// 电量字体：系统字体名，默认 "Segoe UI"（M9；缺失时 GDI 静默回退）。
    pub icon_font_family: String,
    /// 低电提醒阈值（%），默认 20，范围 5..=50（M12 单阈值）。
    pub low_threshold: u8,
    /// 低电闪烁提醒开关，默认开（M11）。
    pub blink_enabled: bool,
    /// 按电量分档着色（v0.3.5 #5：默认开——设置窗口「自动(按电量档位)」
    /// 即分档；由字体颜色下拉派生，不再有独立勾选）。启用时单一字色被忽略。
    pub icon_font_tiers_enabled: bool,
    /// 分档边界（N 档 = N-1 个阈值，默认 [20, 50]）；阈值含该值归上一档。
    pub icon_font_tier_thresholds: Vec<u8>,
    /// 分档颜色（N 个，默认红/黄/绿）。
    pub icon_font_tier_colors: Vec<RgbColor>,
    /// 图标文本大小（% 相对基准字号，D-48/D-57），默认 100，范围 40..=160。
    pub icon_text_scale: u8,
    /// 图标背景覆盖度（% 相对位图内切块，D-48/D-57），默认 100，范围
    /// 20..=100（100% = 满出血零边距，已触及位图边界，故上限为 100）。
    pub icon_bg_scale: u8,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            autostart: false,
            poll_interval_secs: DEFAULT_POLL_SECS,
            icon_style: IconStyle::Auto,
            notifications_enabled: true,
            notification_silent: false,
            icon_shape: IconShape::Rounded,
            icon_background: IconBackground::Dark,
            icon_font_color: IconFontColor::Auto,
            icon_font_family: "Segoe UI".to_string(),
            low_threshold: DEFAULT_LOW_THRESHOLD,
            blink_enabled: true,
            // v0.3.5 #5（D-56）：字体颜色「自动(按电量档位)」即分档，默认开。
            icon_font_tiers_enabled: true,
            icon_font_tier_thresholds: DEFAULT_FONT_TIER_THRESHOLDS.to_vec(),
            icon_font_tier_colors: DEFAULT_FONT_TIER_COLORS.to_vec(),
            icon_text_scale: 100,
            icon_bg_scale: 100,
        }
    }
}

/// 一条配置警告（键 + 原因），进入诊断观测流。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigWarning {
    pub key: String,
    pub reason: String,
}

impl fmt::Display for ConfigWarning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "config key {:?}: {}", self.key, self.reason)
    }
}

enum Value {
    Bool(bool),
    Int(u64),
    Str(String),
    /// `[a, b, c]` 数组（v0.3.3 #4：分档阈值 / 颜色）。
    List(Vec<Value>),
}

fn parse_value(raw: &str) -> Option<Value> {
    let v = raw.trim();
    match v {
        "true" => return Some(Value::Bool(true)),
        "false" => return Some(Value::Bool(false)),
        _ => {}
    }
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        return Some(Value::Str(v[1..v.len() - 1].to_string()));
    }
    if let Ok(n) = v.parse::<u64>() {
        return Some(Value::Int(n));
    }
    // 数组：`[` 开头 `]` 结尾，逗号分隔的标量（v0.3.3 #4）。
    if v.len() >= 2 && v.starts_with('[') && v.ends_with(']') {
        let inner = &v[1..v.len() - 1];
        let mut items = Vec::new();
        for part in inner.split(',') {
            let p = part.trim();
            if p.is_empty() {
                continue;
            }
            items.push(parse_value(p)?);
        }
        return Some(Value::List(items));
    }
    None
}

/// 去除行内注释（`#` 不在引号内时截断）。
fn strip_comment(line: &str) -> &str {
    let mut in_str = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '#' if !in_str => return &line[..i],
            _ => {}
        }
    }
    line
}

impl Config {
    /// 容错解析：返回 (配置, 警告列表)。任何单行问题都不影响其它键。
    pub fn parse(text: &str) -> (Self, Vec<ConfigWarning>) {
        let mut cfg = Config::default();
        let mut warnings = Vec::new();
        let mut warn = |key: &str, reason: &str| {
            warnings.push(ConfigWarning {
                key: key.into(),
                reason: reason.into(),
            })
        };

        let mut saw_low_threshold = false;
        let mut migrated_warn_threshold: Option<u64> = None;
        let mut saw_tier_thresholds = false;

        for raw_line in text.trim_start_matches('\u{FEFF}').lines() {
            let line = strip_comment(raw_line).trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('[') {
                warn(line, "sections are not supported; line ignored");
                continue;
            }
            let Some((key, value_raw)) = line.split_once('=') else {
                warn(line, "not a key = value line; ignored");
                continue;
            };
            let key = key.trim();
            let Some(value) = parse_value(value_raw) else {
                warn(key, "unparseable value; default kept");
                continue;
            };
            match (key, &value) {
                ("autostart", Value::Bool(b)) => cfg.autostart = *b,
                ("poll_interval_secs", Value::Int(n)) => {
                    // D-45：钳制 [60, 1800] + 吸附最近档位（平局取更低档）。
                    let norm = normalize_poll_secs(*n);
                    if norm != *n {
                        if *n < POLL_MIN_SECS || *n > POLL_MAX_SECS {
                            warn(
                                key,
                                &format!("out of range {POLL_MIN_SECS}..={POLL_MAX_SECS}; clamped {n} -> {norm}"),
                            );
                        } else {
                            warn(
                                key,
                                &format!(
                                    "not a supported tier (one of {POLL_TIERS_SECS:?}); snapped {n} -> {norm}"
                                ),
                            );
                        }
                    }
                    cfg.poll_interval_secs = norm;
                }
                ("icon_style", Value::Str(s)) => match s.as_str() {
                    "auto" => cfg.icon_style = IconStyle::Auto,
                    "light" => cfg.icon_style = IconStyle::Light,
                    "dark" => cfg.icon_style = IconStyle::Dark,
                    _ => warn(key, "unknown icon_style; default kept"),
                },
                ("notifications_enabled", Value::Bool(b)) => cfg.notifications_enabled = *b,
                ("notification_silent", Value::Bool(b)) => cfg.notification_silent = *b,
                ("icon_shape", Value::Str(s)) => match s.as_str() {
                    "rounded" => cfg.icon_shape = IconShape::Rounded,
                    "square" => cfg.icon_shape = IconShape::Square,
                    "transparent" => cfg.icon_shape = IconShape::Transparent,
                    _ => warn(key, "unknown icon_shape; default kept"),
                },
                ("icon_background", Value::Str(s)) => {
                    if s.eq_ignore_ascii_case("transparent") {
                        // v0.2.0 透明语义迁移到 icon_shape（M9）。
                        cfg.icon_shape = IconShape::Transparent;
                        warn(
                            key,
                            "\"transparent\" moved to icon_shape; icon_shape set to transparent",
                        );
                    } else if s == "light" {
                        cfg.icon_background = IconBackground::Light;
                    } else if s == "dark" {
                        cfg.icon_background = IconBackground::Dark;
                    } else if let Some(c) = RgbColor::parse_hex(s) {
                        cfg.icon_background = IconBackground::Custom(c);
                    } else {
                        warn(
                            key,
                            "invalid icon_background (light/dark/#RRGGBB); default kept",
                        );
                    }
                }
                ("icon_font_color", Value::Str(s)) => {
                    if s.eq_ignore_ascii_case("auto") {
                        cfg.icon_font_color = IconFontColor::Auto;
                    } else if let Some(c) = RgbColor::parse_hex(s) {
                        cfg.icon_font_color = IconFontColor::Fixed(c);
                    } else {
                        warn(
                            key,
                            r##"invalid icon_font_color (expect "auto" or "#RRGGBB"); default kept"##,
                        );
                    }
                }
                ("icon_font_family", Value::Str(s)) => {
                    let name = s.trim();
                    if name.is_empty() {
                        warn(key, "empty font name; default kept");
                    } else {
                        cfg.icon_font_family = name.to_string();
                    }
                }
                ("low_threshold", Value::Int(n)) => {
                    saw_low_threshold = true;
                    let clamped = (*n).clamp(LOW_THRESHOLD_MIN as u64, LOW_THRESHOLD_MAX as u64);
                    if clamped != *n {
                        warn(key, &format!("out of range; clamped {n} -> {clamped}"));
                    }
                    cfg.low_threshold = clamped as u8;
                }
                ("blink_enabled", Value::Bool(b)) => cfg.blink_enabled = *b,
                ("icon_text_scale", Value::Int(n)) => {
                    // D-48：越界钳制到 [60, 140] + 警告。
                    let clamped =
                        (*n).clamp(ICON_TEXT_SCALE_MIN as u64, ICON_TEXT_SCALE_MAX as u64);
                    if clamped != *n {
                        warn(key, &format!("out of range; clamped {n} -> {clamped}"));
                    }
                    cfg.icon_text_scale = clamped as u8;
                }
                ("icon_bg_scale", Value::Int(n)) => {
                    // D-48：越界钳制到 [60, 100] + 警告（>100 会溢出位图）。
                    let clamped = (*n).clamp(ICON_BG_SCALE_MIN as u64, ICON_BG_SCALE_MAX as u64);
                    if clamped != *n {
                        warn(key, &format!("out of range; clamped {n} -> {clamped}"));
                    }
                    cfg.icon_bg_scale = clamped as u8;
                }
                ("icon_font_tiers_enabled", Value::Bool(b)) => {
                    cfg.icon_font_tiers_enabled = *b;
                }
                ("icon_font_tier_thresholds", Value::List(items)) => {
                    let mut ths = Vec::with_capacity(items.len());
                    let mut ok = true;
                    for it in items {
                        if let Value::Int(n) = it {
                            if *n > u8::MAX as u64 {
                                warn(key, "threshold out of range; default kept");
                                ok = false;
                                break;
                            }
                            ths.push(*n as u8);
                        } else {
                            warn(key, "array items must be integers; default kept");
                            ok = false;
                            break;
                        }
                    }
                    if ok {
                        cfg.icon_font_tier_thresholds = ths;
                        saw_tier_thresholds = true;
                    }
                }
                ("icon_font_tier_colors", Value::List(items)) => {
                    let mut cols = Vec::with_capacity(items.len());
                    let mut ok = true;
                    for it in items {
                        if let Value::Str(s) = it {
                            match RgbColor::parse_hex(s) {
                                Some(c) => cols.push(c),
                                None => warn(
                                    key,
                                    &format!("invalid color {s:?} skipped (expect \"#RRGGBB\")"),
                                ),
                            }
                        } else {
                            warn(key, "array items must be color strings; default kept");
                            ok = false;
                            break;
                        }
                    }
                    if ok {
                        cfg.icon_font_tier_colors = cols;
                    }
                }
                // 废弃键（M12 单阈值取代双阈值）。
                ("low_threshold_warn", Value::Int(n)) if *n <= 100 => {
                    migrated_warn_threshold = Some(*n);
                    warn(key, "deprecated; use low_threshold (value migrated)");
                }
                ("low_threshold_warn", _) => {
                    warn(key, "deprecated and invalid; ignored");
                }
                ("low_threshold_critical", _) => {
                    warn(key, "deprecated (single low_threshold); ignored");
                }
                (
                    "autostart"
                    | "poll_interval_secs"
                    | "icon_style"
                    | "notifications_enabled"
                    | "notification_silent"
                    | "icon_shape"
                    | "icon_background"
                    | "icon_font_color"
                    | "icon_font_family"
                    | "low_threshold"
                    | "blink_enabled"
                    | "icon_text_scale"
                    | "icon_bg_scale"
                    | "icon_font_tiers_enabled"
                    | "icon_font_tier_thresholds"
                    | "icon_font_tier_colors",
                    _,
                ) => warn(key, "invalid value or out of range; default kept"),
                (other, _) => warn(other, "unknown key; ignored"),
            }
        }

        // 废弃键迁移：显式 low_threshold 优先，否则取 warn 阈值（M12）。
        if !saw_low_threshold {
            if let Some(w) = migrated_warn_threshold {
                let clamped = w.clamp(LOW_THRESHOLD_MIN as u64, LOW_THRESHOLD_MAX as u64);
                cfg.low_threshold = clamped as u8;
            }
        }
        // v0.3.5 #5 迁移（D-56）：旧配置「分档勾选关 + 字体颜色 auto」的语义
        // 是「按电量自动变色」，对应新下拉的「自动(按电量档位)」——启用分档，
        // 而不是按字面映射成「自定义」（那会把按电量变化退化成固定色）。
        // 未显式给分档阈值且 low_threshold 定制过时，沿用 [low_threshold, 50]
        // 保住旧的变色点；勾选关 + 固定色 → 自定义（不启用分档）。
        if !cfg.icon_font_tiers_enabled && cfg.icon_font_color == IconFontColor::Auto {
            cfg.icon_font_tiers_enabled = true;
            if !saw_tier_thresholds && cfg.low_threshold != DEFAULT_FONT_TIER_THRESHOLDS[0] {
                cfg.icon_font_tier_thresholds =
                    vec![cfg.low_threshold, DEFAULT_FONT_TIER_THRESHOLDS[1]];
            }
            warnings.push(ConfigWarning {
                key: "icon_font_tiers_enabled".into(),
                reason: "migrated (v0.3.5): font color \"auto\" merged into tier mode; \
                         tiers enabled to preserve per-battery-level coloring"
                    .into(),
            });
        }
        // 分档数据始终规范化（v0.3.3 #4）：钳制 / 递进修复 / 数量对齐。
        {
            let (ths, cols, mut tier_warns) = normalize_font_tiers(
                std::mem::take(&mut cfg.icon_font_tier_thresholds),
                std::mem::take(&mut cfg.icon_font_tier_colors),
            );
            cfg.icon_font_tier_thresholds = ths;
            cfg.icon_font_tier_colors = cols;
            warnings.append(&mut tier_warns);
        }
        (cfg, warnings)
    }

    /// 序列化为扁平 TOML（首次运行生成默认配置 / 设置窗口持久化用）。
    pub fn to_toml(&self) -> String {
        let tier_thresholds = self
            .icon_font_tier_thresholds
            .iter()
            .map(|t| t.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let tier_colors = self
            .icon_font_tier_colors
            .iter()
            .map(|c| format!("\"{}\"", c.to_hex()))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "# LogiVoltTray 配置（扁平键值；未知键会被忽略并产生诊断警告）\n\
             autostart = {}\n\
             poll_interval_secs = {}\n\
             icon_style = \"{}\"\n\
             notifications_enabled = {}\n\
             notification_silent = {}\n\
             icon_shape = \"{}\"\n\
             icon_background = \"{}\"\n\
             icon_font_color = \"{}\"\n\
             icon_font_family = \"{}\"\n\
             low_threshold = {}\n\
             blink_enabled = {}\n\
             icon_font_tiers_enabled = {}\n\
             icon_font_tier_thresholds = [{}]\n\
             icon_font_tier_colors = [{}]\n\
             icon_text_scale = {}\n\
             icon_bg_scale = {}\n",
            self.autostart,
            self.poll_interval_secs,
            self.icon_style.name(),
            self.notifications_enabled,
            self.notification_silent,
            self.icon_shape.name(),
            self.icon_background.name(),
            self.icon_font_color.name(),
            self.icon_font_family,
            self.low_threshold,
            self.blink_enabled,
            self.icon_font_tiers_enabled,
            tier_thresholds,
            tier_colors,
            self.icon_text_scale,
            self.icon_bg_scale,
        )
    }

    /// 默认配置路径：%APPDATA%\LogiVoltTray\config.toml（无 APPDATA 时退回当前目录）。
    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("LogiVoltTray").join("config.toml")
    }

    /// 读取并解析；文件不存在返回默认配置（不算警告）。
    pub fn load(path: &std::path::Path) -> io::Result<(Self, Vec<ConfigWarning>)> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Self::parse(&text)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok((Self::default(), Vec::new())),
            Err(e) => Err(e),
        }
    }
}

/// mtime 轮询式热重载（蓝图：不引入 notify crate）。
#[derive(Debug)]
pub struct ConfigWatcher {
    path: PathBuf,
    last_mtime: Option<SystemTime>,
}

impl ConfigWatcher {
    pub fn new(path: PathBuf) -> Self {
        let last_mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        ConfigWatcher { path, last_mtime }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// mtime 变化时返回 Some(load 结果)；无变化返回 None。
    pub fn poll(&mut self) -> Option<io::Result<(Config, Vec<ConfigWarning>)>> {
        let mtime = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .ok();
        if mtime == self.last_mtime {
            return None;
        }
        self.last_mtime = mtime;
        Some(Config::load(&self.path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_v030_schema() {
        let c = Config::default();
        assert!(!c.autostart);
        assert_eq!(c.poll_interval_secs, DEFAULT_POLL_SECS);
        assert_eq!(c.poll_interval_secs, 600);
        assert_eq!(c.icon_style, IconStyle::Auto);
        assert!(c.notifications_enabled);
        assert!(!c.notification_silent);
        assert_eq!(c.icon_shape, IconShape::Rounded);
        assert_eq!(c.icon_background, IconBackground::Dark);
        assert_eq!(c.icon_font_color, IconFontColor::Auto);
        assert_eq!(c.icon_font_family, "Segoe UI");
        assert_eq!(c.low_threshold, 20);
        assert!(c.blink_enabled);
        // D-48：文本/背景缩放默认 100%。
        assert_eq!(c.icon_text_scale, 100);
        assert_eq!(c.icon_bg_scale, 100);
    }

    #[test]
    fn parse_full_document() {
        let (c, w) = Config::parse(
            "autostart = true\npoll_interval_secs = 300\nicon_style = \"dark\"\n\
             notifications_enabled = false\nnotification_silent = true\n\
             icon_shape = \"square\"\nicon_background = \"#0A6CFF\"\n\
             icon_font_color = \"#FFD21F\"\nicon_font_family = \"Consolas\"\n\
             low_threshold = 30\nblink_enabled = false\n",
        );
        assert!(w.is_empty(), "{w:?}");
        assert!(c.autostart);
        assert_eq!(c.poll_interval_secs, 300);
        assert_eq!(c.icon_style, IconStyle::Dark);
        assert!(!c.notifications_enabled);
        assert!(c.notification_silent);
        assert_eq!(c.icon_shape, IconShape::Square);
        assert_eq!(
            c.icon_background,
            IconBackground::Custom(RgbColor {
                r: 0x0A,
                g: 0x6C,
                b: 0xFF
            })
        );
        assert_eq!(
            c.icon_font_color,
            IconFontColor::Fixed(RgbColor {
                r: 0xFF,
                g: 0xD2,
                b: 0x1F
            })
        );
        assert_eq!(c.icon_font_family, "Consolas");
        assert_eq!(c.low_threshold, 30);
        assert!(!c.blink_enabled);
    }

    #[test]
    fn unknown_keys_warn_not_fail() {
        let (c, w) = Config::parse("evil_key = true\nlow_threshold = 30\n");
        assert_eq!(c.low_threshold, 30);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].key, "evil_key");
    }

    #[test]
    fn invalid_values_fall_back_with_warning() {
        let (c, w) = Config::parse("icon_style = \"neon\"\nicon_shape = \"hex\"\n");
        assert_eq!(c.icon_style, IconStyle::Auto);
        assert_eq!(c.icon_shape, IconShape::Rounded);
        assert_eq!(w.len(), 2);
    }

    #[test]
    fn out_of_range_values_clamp_with_warning() {
        // M10/M12：越界钳制到边界 + 警告（不重置默认）。
        let (c, w) = Config::parse(
            "poll_interval_secs = 5\nlow_threshold = 80\nicon_font_family = \"  \"\n",
        );
        assert_eq!(c.poll_interval_secs, POLL_MIN_SECS);
        assert_eq!(c.low_threshold, LOW_THRESHOLD_MAX);
        assert_eq!(c.icon_font_family, "Segoe UI");
        assert_eq!(w.len(), 3);
        assert!(w.iter().any(|x| x.key == "poll_interval_secs"));
        assert!(w.iter().any(|x| x.key == "low_threshold"));
    }

    #[test]
    fn legacy_keys_migrate() {
        // v0.2.0 双阈值 → v0.3.0 单阈值：warn 值迁移（显式 low_threshold 优先）。
        let (c, w) = Config::parse("low_threshold_warn = 25\nlow_threshold_critical = 10\n");
        assert_eq!(c.low_threshold, 25);
        assert_eq!(w.len(), 2);
        assert!(w.iter().all(|x| x.key.starts_with("low_threshold_")));

        let (c, _) = Config::parse("low_threshold_warn = 25\nlow_threshold = 40\n");
        assert_eq!(c.low_threshold, 40); // 新键优先

        // warn 值超界 → 钳 5..=50。
        let (c, _) = Config::parse("low_threshold_warn = 3\n");
        assert_eq!(c.low_threshold, LOW_THRESHOLD_MIN);
    }

    #[test]
    fn legacy_transparent_background_migrates_to_shape() {
        let (c, w) = Config::parse("icon_background = \"transparent\"\n");
        assert_eq!(c.icon_shape, IconShape::Transparent);
        // 背景保默认 dark（透明时不渲染背景）。
        assert_eq!(c.icon_background, IconBackground::Dark);
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn icon_color_options_parse_and_fall_back() {
        let (c, w) = Config::parse("icon_background = \"light\"\nicon_font_color = \"auto\"\n");
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c.icon_background, IconBackground::Light);
        assert_eq!(c.icon_font_color, IconFontColor::Auto);

        let (c, w) =
            Config::parse("icon_background = \"midnight\"\nicon_font_color = \"#12345\"\n");
        assert_eq!(c.icon_background, IconBackground::Dark);
        assert_eq!(c.icon_font_color, IconFontColor::Auto);
        assert_eq!(w.len(), 2);

        let (c, w) = Config::parse("icon_font_color = \"#ff00aa\"\n");
        assert!(w.is_empty());
        assert_eq!(
            c.icon_font_color,
            IconFontColor::Fixed(RgbColor {
                r: 0xFF,
                g: 0x00,
                b: 0xAA
            })
        );
        let doc2 = Config {
            icon_font_color: c.icon_font_color,
            ..Config::default()
        }
        .to_toml();
        let (back, w2) = Config::parse(&doc2);
        assert!(w2.is_empty());
        assert_eq!(back.icon_font_color, c.icon_font_color);
    }

    #[test]
    fn comments_and_malformed_lines_tolerated() {
        let (c, w) =
            Config::parse("# comment\n\nlow_threshold = 15 # inline\n[section]\ngarbage line\n");
        assert_eq!(c.low_threshold, 15);
        assert_eq!(w.len(), 2); // section + garbage
    }

    #[test]
    fn toml_roundtrip() {
        let c = Config {
            autostart: true,
            icon_shape: IconShape::Square,
            low_threshold: 35,
            icon_background: IconBackground::Custom(RgbColor {
                r: 0x12,
                g: 0x34,
                b: 0x56,
            }),
            icon_font_family: "Arial".into(),
            ..Config::default()
        };
        let (back, w) = Config::parse(&c.to_toml());
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(back, c);
    }

    #[test]
    fn contrast_and_brighten_math() {
        // 黑白对比 ≈ 21；同色对比 = 1。
        let black_white = contrast_ratio((0, 0, 0), (255, 255, 255));
        assert!((black_white - 21.0).abs() < 0.01, "{black_white}");
        assert!((contrast_ratio((10, 10, 10), (10, 10, 10)) - 1.0).abs() < 1e-9);
        assert!(contrast_ok((255, 255, 255), (0x1F, 0x1F, 0x1F)));
        assert!(!contrast_ok((0x30, 0x30, 0x30), (0x1F, 0x1F, 0x1F)));
        // 提亮 40%（v0.3.4 #3：向白插值）：31 → 31+224×0.4 = 120.6 → 121 (0x79)；
        // 纯白封顶不动；纯红 → (255, 102, 102)（饱和通道也有可见跳变）。
        assert_eq!(brighten((0x1F, 0x1F, 0x1F), 0.4), (0x79, 0x79, 0x79));
        assert_eq!(brighten((255, 255, 255), 0.4), (255, 255, 255));
        assert_eq!(brighten((255, 0, 0), 0.4), (255, 102, 102));
        // flash_rgb：非近白色走 brighten；纯白（对白对比度 1.0 ≤ 1.2）反向
        // 向深灰插值 255×0.6 + 0x1B×0.4 = 163.8 → 164，闪烁两帧必有差异。
        assert_eq!(flash_rgb((255, 0, 0), 0.4), (255, 102, 102));
        assert_eq!(flash_rgb((255, 255, 255), 0.4), (164, 164, 164));
        // 近白浅底 #F2F2F2（对白对比度 ≈1.07）也走暗插值：242×0.6+27×0.4≈155。
        let f = flash_rgb((0xF2, 0xF2, 0xF2), 0.4);
        assert!(f.0 < 200, "{f:?}");
    }

    /// D-45：轮询间隔档位归一化——旧档位与任意值吸附最近档（平局取低）。
    #[test]
    fn poll_secs_snap_to_tiers() {
        // 档位原值不动。
        for t in POLL_TIERS_SECS {
            assert_eq!(normalize_poll_secs(t), t);
        }
        // 被移除的旧档位：15/30/120 → 60。
        assert_eq!(normalize_poll_secs(15), 60);
        assert_eq!(normalize_poll_secs(30), 60);
        assert_eq!(normalize_poll_secs(120), 60);
        // 越界钳制：5 → 60，2400 → 1800。
        assert_eq!(normalize_poll_secs(5), POLL_MIN_SECS);
        assert_eq!(normalize_poll_secs(2400), POLL_MAX_SECS);
        // 非档位值吸附最近档：200 → 300（|200-60|>|200-300|）；700 → 600。
        assert_eq!(normalize_poll_secs(200), 300);
        assert_eq!(normalize_poll_secs(700), 600);
        // 平局取更低档：180 与 60/300 各差 120 → 取 60（更省电）。
        assert_eq!(normalize_poll_secs(180), 60);
        // 解析层：越界与非档位值都产生警告并落位。
        let (c, w) = Config::parse("poll_interval_secs = 120\n");
        assert_eq!(c.poll_interval_secs, 60);
        assert_eq!(w.len(), 1);
        assert!(w[0].reason.contains("snapped"), "{w:?}");
        let (c, w) = Config::parse("poll_interval_secs = 5\n");
        assert_eq!(c.poll_interval_secs, POLL_MIN_SECS);
        assert_eq!(w.len(), 1);
        assert!(w[0].reason.contains("out of range"), "{w:?}");
        // 档位值无警告。
        let (c, w) = Config::parse("poll_interval_secs = 1800\n");
        assert_eq!(c.poll_interval_secs, 1800);
        assert!(w.is_empty(), "{w:?}");
    }

    /// D-48：文本/背景缩放键——解析、钳制、往返。
    #[test]
    fn icon_scale_keys_parse_clamp_roundtrip() {
        let (c, w) = Config::parse("icon_text_scale = 130\nicon_bg_scale = 80\n");
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c.icon_text_scale, 130);
        assert_eq!(c.icon_bg_scale, 80);
        let (back, w2) = Config::parse(&c.to_toml());
        assert!(w2.is_empty(), "{w2:?}");
        assert_eq!(back, c);

        // 越界钳制 + 警告：文本 200 → 160、10 → 40；背景 130 → 100（几何上限）、
        // 10 → 20（D-57 新边界）。
        let (c, w) = Config::parse("icon_text_scale = 200\nicon_bg_scale = 130\n");
        assert_eq!(c.icon_text_scale, ICON_TEXT_SCALE_MAX);
        assert_eq!(c.icon_bg_scale, ICON_BG_SCALE_MAX);
        assert_eq!(w.len(), 2);
        assert!(w.iter().any(|x| x.key == "icon_text_scale"));
        assert!(w.iter().any(|x| x.key == "icon_bg_scale"));
        let (c, w) = Config::parse("icon_text_scale = 10\n");
        assert_eq!(c.icon_text_scale, ICON_TEXT_SCALE_MIN);
        assert_eq!(w.len(), 1);
        let (c, w) = Config::parse("icon_bg_scale = 10\n");
        assert_eq!(c.icon_bg_scale, ICON_BG_SCALE_MIN);
        assert_eq!(w.len(), 1);
        // 非整数 / 布尔 → 默认保留 + 警告。
        let (c, w) = Config::parse("icon_text_scale = true\n");
        assert_eq!(c.icon_text_scale, 100);
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn font_tier_defaults() {
        let c = Config::default();
        // v0.3.5 #5（D-56）：分档默认开（「自动(按电量档位)」即分档）。
        assert!(c.icon_font_tiers_enabled);
        assert_eq!(c.icon_font_tier_thresholds, vec![20, 50]);
        assert_eq!(c.icon_font_tier_colors.len(), 3);
        // 默认值本身必须已合规（规范化幂等、无警告）。
        let (ths, cols, w) = normalize_font_tiers(
            c.icon_font_tier_thresholds.clone(),
            c.icon_font_tier_colors.clone(),
        );
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(ths, c.icon_font_tier_thresholds);
        assert_eq!(cols, c.icon_font_tier_colors);
    }

    #[test]
    fn font_tier_mapping_semantics() {
        // [20, 50]：0..=20 → 0；21..=50 → 1；51..=100 → 2（含边界值归上一档）。
        let th = [20u8, 50];
        assert_eq!(font_tier_for_percent(&th, 0), 0);
        assert_eq!(font_tier_for_percent(&th, 19), 0);
        assert_eq!(font_tier_for_percent(&th, 20), 0);
        assert_eq!(font_tier_for_percent(&th, 21), 1);
        assert_eq!(font_tier_for_percent(&th, 50), 1);
        assert_eq!(font_tier_for_percent(&th, 51), 2);
        assert_eq!(font_tier_for_percent(&th, 100), 2);
        // 用户示例：20 / 50 两个数字框。
        let th2 = [20u8];
        assert_eq!(font_tier_for_percent(&th2, 20), 0);
        assert_eq!(font_tier_for_percent(&th2, 21), 1);
        let th4 = [10u8, 30, 60, 90];
        assert_eq!(font_tier_for_percent(&th4, 0), 0);
        assert_eq!(font_tier_for_percent(&th4, 10), 0);
        assert_eq!(font_tier_for_percent(&th4, 11), 1);
        assert_eq!(font_tier_for_percent(&th4, 60), 2);
        assert_eq!(font_tier_for_percent(&th4, 91), 4);
        assert_eq!(font_tier_for_percent(&th4, 100), 4);
    }

    #[test]
    fn font_tier_parse_and_roundtrip() {
        let (c, w) = Config::parse(
            "icon_font_tiers_enabled = true\n\
             icon_font_tier_thresholds = [15, 45, 80]\n\
             icon_font_tier_colors = [\"#E81123\", \"#FFD21F\", \"#00CC6A\", \"#0078D7\"]\n",
        );
        assert!(w.is_empty(), "{w:?}");
        assert!(c.icon_font_tiers_enabled);
        assert_eq!(c.icon_font_tier_thresholds, vec![15, 45, 80]);
        assert_eq!(c.icon_font_tier_colors.len(), 4);
        // 序列化往返。
        let (back, w2) = Config::parse(&c.to_toml());
        assert!(w2.is_empty(), "{w2:?}");
        assert_eq!(back, c);
    }

    #[test]
    fn font_tier_normalize_clamps_and_repairs() {
        // 越界钳制：0 → 1，150 → 99，随后非递进被抬升 → [1, 99]。
        let (ths, cols, w) = normalize_font_tiers(vec![0, 150, 30], vec![]);
        assert_eq!(ths, vec![1, 99]);
        assert_eq!(cols.len(), 3);
        assert!(!w.is_empty());
        // 非严格递增：[20, 20] → [20, 21]。
        let (ths, _, w) = normalize_font_tiers(vec![20, 20], vec![]);
        assert_eq!(ths, vec![20, 21]);
        assert!(w.iter().any(|x| x.reason.contains("strictly increasing")));
        // 数量越界：单阈值（2 档）与 5 个阈值（6 档）均重置默认 3 档。
        let (ths, _, w) = normalize_font_tiers(vec![], vec![]);
        assert_eq!(ths, vec![20, 50]);
        assert!(w.len() == 1);
        let (ths, cols, w) = normalize_font_tiers(vec![10, 20, 30, 40, 50], vec![]);
        assert_eq!(ths, vec![20, 50]);
        assert_eq!(cols.len(), 3);
        assert!(!w.is_empty());
        // 递进溢出：[99, 5] → 第二个抬到 100 超 99，丢弃余下阈值。
        let (ths, _, _) = normalize_font_tiers(vec![99, 5], vec![]);
        assert_eq!(ths, vec![99]);
        // 颜色对齐：不足补调色板、多余截断。
        let (_, cols, w) = normalize_font_tiers(vec![20, 50], vec![RgbColor { r: 0, g: 0, b: 0 }]);
        assert_eq!(cols.len(), 3);
        assert!(w.iter().any(|x| x.key == "icon_font_tier_colors"));
        let (_, cols, _) = normalize_font_tiers(
            vec![20, 50],
            (0..5)
                .map(|i| RgbColor {
                    r: i as u8,
                    g: 0,
                    b: 0,
                })
                .collect(),
        );
        assert_eq!(cols.len(), 3);
    }

    #[test]
    fn font_tier_parse_repairs_and_warns() {
        // 无效颜色跳过 + 警告；阈值钳制 + 警告；颜色补齐。
        let (c, w) = Config::parse(
            "icon_font_tier_thresholds = [0, 150]\n\
             icon_font_tier_colors = [\"#12345\", \"#E81123\"]\n",
        );
        assert_eq!(c.icon_font_tier_thresholds, vec![1, 99]);
        assert_eq!(c.icon_font_tier_colors.len(), 3);
        assert!(w.iter().any(|x| x.key == "icon_font_tier_thresholds"));
        assert!(w.iter().any(|x| x.key == "icon_font_tier_colors"));
        // 数组元素类型错误 → 整组保默认 + 警告。
        let (c, w) = Config::parse("icon_font_tier_thresholds = [20, \"50\"]\n");
        assert_eq!(c.icon_font_tier_thresholds, vec![20, 50]);
        assert_eq!(w.len(), 1);
        // 数组值给到标量键 → catch-all 警告；v0.3.5 默认开，「保默认」即开。
        let (c, w) = Config::parse("icon_font_tiers_enabled = [true]\n");
        assert!(c.icon_font_tiers_enabled);
        assert_eq!(w.len(), 1);
    }

    /// v0.3.5 #1（D-52）：COLORREF ↔ RGB 字节序，验收用例。
    #[test]
    fn colorref_rgb_byte_order_roundtrip() {
        // 验收：COLORREF 0x000000FF（红）→ 序列化必须是 #FF0000。
        let red = colorref_to_rgb(0x0000_00FF);
        assert_eq!(red.to_hex(), "#FF0000");
        // 反向：#FF0000 → COLORREF 0x000000FF。
        assert_eq!(
            rgb_to_colorref(RgbColor {
                r: 0xFF,
                g: 0x00,
                b: 0x00
            }),
            0x0000_00FF
        );
        // 纯蓝不再被串成红色：COLORREF 0x00FF0000 ↔ #0000FF。
        assert_eq!(colorref_to_rgb(0x00FF_0000).to_hex(), "#0000FF");
        assert_eq!(
            rgb_to_colorref(RgbColor {
                r: 0x00,
                g: 0x00,
                b: 0xFF
            }),
            0x00FF_0000
        );
        // 往返恒等（含高 8 位为零的全部边界）。
        for c in [0x0000_0000u32, 0x00FF_FFFF, 0x0012_3456, 0x00AB_CDEF] {
            assert_eq!(rgb_to_colorref(colorref_to_rgb(c)), c);
            assert_eq!(
                colorref_to_rgb(rgb_to_colorref(colorref_to_rgb(c))),
                colorref_to_rgb(c)
            );
        }
    }

    /// v0.3.5 #4（D-55）：透明背景闪烁帧的文本透明度脉冲。
    #[test]
    fn blink_text_alpha_pulse_math() {
        // 255 × (1 − 0.55) = 114.75 → 115：与普通帧 255 差异肉眼可见。
        assert_eq!(pulse_alpha(255, BLINK_TEXT_ALPHA_PULSE), 115);
        // 普通帧不脉冲（factor = 0 恒等）。
        assert_eq!(pulse_alpha(255, 0.0), 255);
        // 脉冲只降不升，且永不超原值。
        for a in [0u8, 1, 128, 254, 255] {
            assert!(pulse_alpha(a, BLINK_TEXT_ALPHA_PULSE) <= a);
        }
    }

    /// v0.3.5 #5（D-56）：旧配置「分档勾选关 + auto」迁移为分档开。
    #[test]
    fn v035_auto_font_color_migrates_to_tier_mode() {
        // 勾选关 + 显式 auto → 启用分档 + 迁移警告；默认阈值不变。
        let (c, w) = Config::parse("icon_font_tiers_enabled = false\nicon_font_color = \"auto\"\n");
        assert!(c.icon_font_tiers_enabled);
        assert_eq!(c.icon_font_tier_thresholds, vec![20, 50]);
        assert!(
            w.iter()
                .any(|x| x.key == "icon_font_tiers_enabled" && x.reason.contains("migrated")),
            "{w:?}"
        );
        // 勾选关 + 未写 font_color（默认 auto）+ 定制 low_threshold → 沿用
        // [low_threshold, 50]，保住旧的变色点。
        let (c, _) = Config::parse("icon_font_tiers_enabled = false\nlow_threshold = 30\n");
        assert!(c.icon_font_tiers_enabled);
        assert_eq!(c.icon_font_tier_thresholds, vec![30, 50]);
        // 显式分档阈值优先，不被迁移种子覆盖。
        let (c, _) = Config::parse(
            "icon_font_tiers_enabled = false\nlow_threshold = 30\nicon_font_tier_thresholds = [10, 40]\n",
        );
        assert!(c.icon_font_tiers_enabled);
        assert_eq!(c.icon_font_tier_thresholds, vec![10, 40]);
        // 勾选关 + 固定色 → 自定义：不启用分档、无迁移警告。
        let (c, w) =
            Config::parse("icon_font_tiers_enabled = false\nicon_font_color = \"#FF0000\"\n");
        assert!(!c.icon_font_tiers_enabled);
        assert!(w.is_empty(), "{w:?}");
        // 勾选开 → 无迁移警告（本来就在分档模式）。
        let (c, w) = Config::parse("icon_font_tiers_enabled = true\n");
        assert!(c.icon_font_tiers_enabled);
        assert!(w.is_empty(), "{w:?}");
        // 空配置 = 全新默认：分档开、无警告。
        let (c, w) = Config::parse("");
        assert!(c.icon_font_tiers_enabled);
        assert!(w.is_empty(), "{w:?}");
    }
}
