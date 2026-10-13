//! 配置 schema（v0.3.0 设置体系 M9-M13）+ 容错解析 + mtime 热重载。
//!
//! 配置文件：`%APPDATA%\LogiVoltTray\config.toml`（扁平键值，无 section）。
//! 解析器为扁平 TOML 子集（bool / 整数 / 字符串），见 docs/decisions.md D-07。
//! 容错规则：未知键 / 非法值 → 回退默认 + 诊断警告；越界值 → **钳制到边界 +
//! 警告**（M10：不再重置默认）；绝不 panic、绝不拒绝启动。
//!
//! 键位一览（v0.3.3，共 14 键；前 11 键同 v0.3.0）：
//! | 键 | 类型 | 默认 | 取值范围 / 说明 |
//! | --- | --- | --- | --- |
//! | `autostart` | bool | `false` | 开机自启（写入启动快捷方式） |
//! | `poll_interval_secs` | int | `60` | `15`–`300`，轮询间隔（越界钳制 + 警告） |
//! | `icon_style` | str | `"auto"` | `auto`/`light`/`dark`（整体风格提示键，v0.3.0 不参与渲染） |
//! | `notifications_enabled` | bool | `true` | 低电量通知开关 |
//! | `notification_silent` | bool | `false` | 通知静默（无提示音） |
//! | `icon_shape` | str | `"rounded"` | `rounded` / `square` / `transparent`（M9） |
//! | `icon_background` | str | `"dark"` | `light` / `dark` / `"#RRGGBB"`（预设或自定义；`transparent` 迁移到 `icon_shape`，M9） |
//! | `icon_font_color` | str | `"auto"` | `"auto"` 或 `"#RRGGBB"`（M9；分档模式启用时被忽略） |
//! | `icon_font_family` | str | `"Segoe UI"` | 系统字体名（渲染时缺失则 GDI 静默回退） |
//! | `low_threshold` | int | `20` | `5`–`50`，低电提醒阈值（M12 单阈值，越界钳制） |
//! | `blink_enabled` | bool | `true` | 低电闪烁提醒（M11，可关） |
//! | `icon_font_tiers_enabled` | bool | `false` | 按电量分档着色（v0.3.3 #4；启用时单一字色入口隐藏） |
//! | `icon_font_tier_thresholds` | int 数组 | `[20, 50]` | N-1 个边界（N 档 2..=5）；阈值含该值归上一档；1–99 严格递增（越界钳制 / 递进修复 + 警告） |
//! | `icon_font_tier_colors` | str 数组 | `["#E81123", "#FFD21F", "#00CC6A"]` | N 个分档色（缺失补默认色、多余截断 + 警告） |
//!
//! 废弃键（保留迁移语义）：
//! - `low_threshold_warn`：迁移为 `low_threshold`（钳 5..=50）+ 警告；
//! - `low_threshold_critical`：忽略 + 警告（M12 单阈值取代双阈值）。

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::time::SystemTime;

pub const DEFAULT_POLL_SECS: u64 = 60;
pub const POLL_MIN_SECS: u64 = 15;
pub const POLL_MAX_SECS: u64 = 300;
pub const DEFAULT_LOW_THRESHOLD: u8 = 20;
pub const LOW_THRESHOLD_MIN: u8 = 5;
pub const LOW_THRESHOLD_MAX: u8 = 50;
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
    let mut warn = |reason: String| warnings.push(ConfigWarning {
        key: "icon_font_tier_thresholds".into(),
        reason,
    });

    let tier_count = thresholds.len() + 1;
    if tier_count < FONT_TIER_MIN_TIERS || tier_count > FONT_TIER_MAX_TIERS {
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
            reason: format!(
                "too many colors; truncated to {tier_count}"
            ),
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

/// 提亮（M11 闪烁：背景提亮约 40%，透明底时提亮字色）。
pub fn brighten((r, g, b): (u8, u8, u8), factor: f64) -> (u8, u8, u8) {
    let f = |v: u8| -> u8 { ((v as f64 * (1.0 + factor)).min(255.0) + 0.5) as u8 };
    (f(r), f(g), f(b))
}

/// 配置（v0.3.0：11 键 + 废弃迁移）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Config {
    /// 开机自启（HKCU Run 单值）。
    pub autostart: bool,
    /// 轮询间隔（秒），默认 60，范围 15..=300（越界钳制 + 警告）。
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
    /// 按电量分档着色（v0.3.3 #4）；启用时单一字色被忽略、设置窗口隐藏单一字色入口。
    pub icon_font_tiers_enabled: bool,
    /// 分档边界（N 档 = N-1 个阈值，默认 [20, 50]）；阈值含该值归上一档。
    pub icon_font_tier_thresholds: Vec<u8>,
    /// 分档颜色（N 个，默认红/黄/绿）。
    pub icon_font_tier_colors: Vec<RgbColor>,
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
            icon_font_tiers_enabled: false,
            icon_font_tier_thresholds: DEFAULT_FONT_TIER_THRESHOLDS.to_vec(),
            icon_font_tier_colors: DEFAULT_FONT_TIER_COLORS.to_vec(),
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
                    // M10：越界钳制到 [15, 300] + 警告（不再重置默认）。
                    let clamped = (*n).clamp(POLL_MIN_SECS, POLL_MAX_SECS);
                    if clamped != *n {
                        warn(key, &format!("out of range; clamped {n} -> {clamped}"));
                    }
                    cfg.poll_interval_secs = clamped;
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
             icon_font_tier_colors = [{}]\n",
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
        assert_eq!(c.poll_interval_secs, 60);
        assert_eq!(c.icon_style, IconStyle::Auto);
        assert!(c.notifications_enabled);
        assert!(!c.notification_silent);
        assert_eq!(c.icon_shape, IconShape::Rounded);
        assert_eq!(c.icon_background, IconBackground::Dark);
        assert_eq!(c.icon_font_color, IconFontColor::Auto);
        assert_eq!(c.icon_font_family, "Segoe UI");
        assert_eq!(c.low_threshold, 20);
        assert!(c.blink_enabled);
    }

    #[test]
    fn parse_full_document() {
        let (c, w) = Config::parse(
            "autostart = true\npoll_interval_secs = 30\nicon_style = \"dark\"\n\
             notifications_enabled = false\nnotification_silent = true\n\
             icon_shape = \"square\"\nicon_background = \"#0A6CFF\"\n\
             icon_font_color = \"#FFD21F\"\nicon_font_family = \"Consolas\"\n\
             low_threshold = 30\nblink_enabled = false\n",
        );
        assert!(w.is_empty(), "{w:?}");
        assert!(c.autostart);
        assert_eq!(c.poll_interval_secs, 30);
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
        // 提亮 40%：31×1.4≈43.4 四舍五入 → 43 (0x2B)；封顶 255。
        assert_eq!(brighten((0x1F, 0x1F, 0x1F), 0.4), (0x2B, 0x2B, 0x2B));
        assert_eq!(brighten((255, 200, 0), 0.4), (255, 255, 0));
    }

    #[test]
    fn font_tier_defaults() {
        let c = Config::default();
        assert!(!c.icon_font_tiers_enabled);
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
        let (_, cols, w) = normalize_font_tiers(vec![20, 50], vec![RgbColor {
            r: 0,
            g: 0,
            b: 0,
        }]);
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
        // 数组值给到标量键 → catch-all 警告。
        let (c, w) = Config::parse("icon_font_tiers_enabled = [true]\n");
        assert!(!c.icon_font_tiers_enabled);
        assert_eq!(w.len(), 1);
    }
}
