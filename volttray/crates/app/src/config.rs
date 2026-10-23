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
//! | `poll_interval_secs` | int | `1800` | 档位 `{300, 600, 1800, 3600, 7200}`（D-66；越界钳制 / 非档位吸附最近档 + 警告） |
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
//! | `icon_text_scale` | int | `140` | `40`–`160`，图标文本大小（%，D-48/D-57 范围拓展，越界钳制；v0.3.18 默认 100→140，U-1） |
//! | `icon_text_scale_migrated_v0318` | bool | `true` | v0.3.18 默认值迁移标记（U-1）：缺失且 scale 恰为旧默认 100 → 迁 140 + 警告 |
//! | `icon_bg_scale` | int | `100` | `20`–`100`，图标背景覆盖度（%，D-57；100% = 满出血零边距，几何上限） |
//!
//! `icon_text_scale` 百分数基准语义（v0.3.18 查证并冻结，R4 BF-3）：
//! 100% = 字高 = 托盘位图边长 ×5/8（≥2 字符）/ ×3/4（单字符）——v0.3.3
//! 起冻结的几何，与系统字号/DPI 无关；位图边长随 DPI 档位缩放
//! （96dpi→16px，100% 档两位数文本字高仅 10px，是「默认偏小」的根因）；
//! 96dpi 下两位数文本的有效上限 ≈140%（再大触发按背景块宽度的缩字）。
//!
//! 废弃键（保留迁移语义）：
//! - `low_threshold_warn`：迁移为 `low_threshold`（钳 5..=50）+ 警告；
//! - `low_threshold_critical`：忽略 + 警告（M12 单阈值取代双阈值）。

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::time::SystemTime;

/// 轮询间隔默认值（D-66：1800 秒 / 30 分钟——实测轮询本身的鼠标电耗可忽略
/// （一次 HID++ 电量查询仅约 2 个射频短包，60s 轮询一天 ≈ 活跃使用约 3 秒的
/// 无线电耗），间隔只需保证显示足够新鲜；1800s ≈ 耗电最快游戏鼠标 50% 电量
/// 时长 X≈33 分钟，办公鼠标任意档都远快于其 X；低电提醒在阈值 ≥5% 时
/// 30 分钟级延迟完全可接受）。
pub const DEFAULT_POLL_SECS: u64 = 1800;
pub const POLL_MIN_SECS: u64 = 300;
pub const POLL_MAX_SECS: u64 = 7200;
/// 轮询档位（D-66）：设置窗口与配置解析共用；60/900 秒两档已移除（X 跨设备
/// 差 30 倍，无法用固定值精确命中「50%X」，档位按「显示新鲜度足够」排布）；
/// 存量配置的 60/900 由 `normalize_poll_secs` 迁移到最近新档（60→300、
/// 900→600）并写警告日志。
pub const POLL_TIERS_SECS: [u64; 5] = [300, 600, 1800, 3600, 7200];
pub const DEFAULT_LOW_THRESHOLD: u8 = 20;
pub const LOW_THRESHOLD_MIN: u8 = 5;
pub const LOW_THRESHOLD_MAX: u8 = 50;
/// 图标文本大小下限（%，D-48；D-57 由 60 拓展到 40）。
pub const ICON_TEXT_SCALE_MIN: u8 = 40;
/// 图标文本大小上限（%，D-48；D-57 由 140 拓展到 160）。
pub const ICON_TEXT_SCALE_MAX: u8 = 160;
/// 图标文本大小默认值（%，v0.3.18 U-1）：100% 档在 96dpi（位图 16px）
/// 下字高仅 10px（边长×5/8），用户反馈默认偏小、日常使用顶格 160；
/// 140% ≈ 96dpi 下两位数文本的有效上限（字高 14px 恰好顶满背景块
/// 15/16 安全边，再大即触发缩字），默认值一步到位、不浪费档位。
pub const ICON_TEXT_SCALE_DEFAULT: u8 = 140;
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

/// 轮询间隔归一化（D-66）：先钳制到 [300, 7200]，再吸附到最近档位；
/// 距离相同取更低档。存量配置中被移除的 60/900 秒档由此归位（60 → 300，
/// 900 → 600），更早移除的 15/30/120 秒档同样吸附（→ 300）。设置窗口只写
/// 档位值，此函数保证 TOML 手改任意值也落在档位上（设置窗口下拉才能正确回显）。
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
    /// 轮询间隔（秒），默认 1800（D-66），解析归一到档位
    /// {300, 600, 1800, 3600, 7200}（越界钳制 / 非档位吸附最近档 + 警告）。
    pub poll_interval_secs: u64,
    /// 整体风格提示键（v0.2.0 遗留，v0.3.0 不参与渲染）。
    pub icon_style: IconStyle,
    /// 通知总开关（仅管辖 toast 弹窗；不影响图标闪烁，闪烁由 blink_enabled 管辖）。
    pub notifications_enabled: bool,
    /// 通知静音（仅关闭 toast 提示音；不抑制弹窗，与图标闪烁无关）。
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
    /// 图标文本大小（% 相对基准字号，D-48/D-57；基准语义见模块文档），
    /// 默认 140（v0.3.18 U-1，原 100），范围 40..=160。
    pub icon_text_scale: u8,
    /// v0.3.18（U-1）默认值迁移标记：缺失（旧配置）且 icon_text_scale
    /// 恰为旧默认 100 → 视为「从未调整过」迁到 140 + 警告；标记存在即
    /// 不再迁移。解析后恒置 true 并随 to_toml 写出；旧版本读取时按
    /// 未知键忽略并警告（降级兼容，R4 BF-2/BF-7 口径）。
    pub icon_text_scale_migrated_v0318: bool,
    /// 图标背景覆盖度（% 相对位图内切块，D-48/D-57），默认 100，范围
    /// 20..=100（100% = 满出血零边距，已触及位图边界，故上限为 100）。
    pub icon_bg_scale: u8,
    /// 实验性功能开关组（v0.3.22 起称「实验室（试验功能）」，D-87）：
    /// 四项未真机验证功能的逐项总闸。**v0.3.22 起新装默认开**（D-85，
    /// Default 全 true，相位二冲突三口径修订）；存量配置显式解析到的
    /// 键值（含 false）不被覆盖（C-09：回退→再升级 / 手写存量配置
    /// 路径下显式 false 保留），无 v0321 标记的更早存量经迁移补 true
    /// 保留原行为。尚未完成全量真机验证——如遇异常可到
    /// 设置→精细化设置 逐项关闭并通过 Issues 反馈。
    /// 重启即显最后已知电量（last_known.json 持久化 + 虚拟条目）。
    pub experimental_persist_last_known: bool,
    /// 耗电统计与导出（stats.jsonl 落盘与导出链路）。
    pub experimental_stats: bool,
    /// 按设备计时轮询——关 = 充电设备不再走 15s 快刷档、与常态设备同
    /// 按配置档轮询。休眠设备零主动查询（D-33 红线）不受本开关影响，
    /// 恒保持。
    pub experimental_per_device_polling: bool,
    /// 蓝牙直连与有线设备读取（HID++ 0xFF 直连槽扫描）。
    pub experimental_bluetooth_wired: bool,
    /// v0.3.21 存量迁移标记（先例：icon_text_scale_migrated_v0318）：
    /// 缺失（v0.3.20 及更早配置）→ 四个 experimental_* 缺失键迁 true
    /// 保留原行为 + 警告（显式解析到的键不覆盖，C-09）；标记存在 →
    /// 按文件显式值。解析后恒置 true 并随 to_toml 写出（幂等）；
    /// 旧版本读取时按未知键忽略（降级兼容）。
    pub experimental_migrated_v0321: bool,
    /// v0.3.22（D-87）：设置窗口「精细化设置」闸门展开态——true=展开
    /// 「实验室（试验功能）」分组，false=收起。**收起仅隐藏选项，不影
    /// 响四键使能**（闸门只管可见性、不管使能）；随 to_toml 物化，旧
    /// 版本读取时按未知键忽略 + 告警、不拒启（沿既有降级先例）。
    pub ui_fine_tune_expanded: bool,
    /// v0.3.22（D-86）：Q3 一次性提示闩锁——true=尚未提示（有资格），
    /// 提示后取走置 false 并持久化（仿 D-64 取走即消）；不参与迁移
    /// 逻辑、不随 v0321 标记联动。
    pub ui_default_on_notice_v0322: bool,
    /// 运行时推导字段（**不序列化、不入 to_toml、不从文件解析**）：本
    /// 轮 parse 的文件里是否显式出现 v0321 标记。Default 置 true 使
    /// to_toml roundtrip 等价成立（新装四键全开 → Q3 判定恒 false，
    /// 该取值无行为影响）。
    pub experimental_marker_in_file: bool,
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
            icon_text_scale: ICON_TEXT_SCALE_DEFAULT,
            icon_text_scale_migrated_v0318: true,
            icon_bg_scale: 100,
            // D-85（2026-10-23，项目所有者终审）：实验室四键新装默认开
            // （v0.3.21 存量经 experimental_migrated_v0321 迁移/显式值
            // 保留原状，不翻转）。
            experimental_persist_last_known: true,
            experimental_stats: true,
            experimental_per_device_polling: true,
            experimental_bluetooth_wired: true,
            experimental_migrated_v0321: true,
            // D-87：精细化设置闸门默认收起（只影响可见性）。
            ui_fine_tune_expanded: false,
            // D-86：Q3 一次性提示闩锁默认未取走（有提示资格）。
            ui_default_on_notice_v0322: true,
            // 运行时推导字段（见结构体注释；新装不经 parse，恒无行为影响）。
            experimental_marker_in_file: true,
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
        let mut saw_text_scale_marker = false;
        let mut saw_experimental_marker = false;
        // C-09：逐键记录显式出现——迁移只补缺失键、不覆盖显式解析到的键
        // （回退→再升级 / 手写存量配置路径，显式 false 不得被重置）。
        let mut saw_experimental_persist = false;
        let mut saw_experimental_stats = false;
        let mut saw_experimental_per_device = false;
        let mut saw_experimental_bluetooth = false;

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
                    // D-66：钳制 [300, 7200] + 吸附最近档位（平局取更低档）。
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
                    // D-57：越界钳制到 [40, 160] + 警告。
                    let clamped =
                        (*n).clamp(ICON_TEXT_SCALE_MIN as u64, ICON_TEXT_SCALE_MAX as u64);
                    if clamped != *n {
                        warn(key, &format!("out of range; clamped {n} -> {clamped}"));
                    }
                    cfg.icon_text_scale = clamped as u8;
                }
                ("icon_text_scale_migrated_v0318", Value::Bool(b)) => {
                    saw_text_scale_marker = true;
                    cfg.icon_text_scale_migrated_v0318 = *b;
                }
                // 相位二（冲突三）：实验功能开关——显式键按文件值。
                ("experimental_persist_last_known", Value::Bool(b)) => {
                    saw_experimental_persist = true;
                    cfg.experimental_persist_last_known = *b;
                }
                ("experimental_stats", Value::Bool(b)) => {
                    saw_experimental_stats = true;
                    cfg.experimental_stats = *b;
                }
                ("experimental_per_device_polling", Value::Bool(b)) => {
                    saw_experimental_per_device = true;
                    cfg.experimental_per_device_polling = *b;
                }
                ("experimental_bluetooth_wired", Value::Bool(b)) => {
                    saw_experimental_bluetooth = true;
                    cfg.experimental_bluetooth_wired = *b;
                }
                ("experimental_migrated_v0321", Value::Bool(b)) => {
                    saw_experimental_marker = true;
                    cfg.experimental_migrated_v0321 = *b;
                }
                // v0.3.22（D-86/D-87）：UI 辅助键——显式键按文件值，
                // 不参与迁移逻辑。
                ("ui_fine_tune_expanded", Value::Bool(b)) => {
                    cfg.ui_fine_tune_expanded = *b;
                }
                ("ui_default_on_notice_v0322", Value::Bool(b)) => {
                    cfg.ui_default_on_notice_v0322 = *b;
                }
                ("icon_bg_scale", Value::Int(n)) => {
                    // D-57：越界钳制到 [20, 100] + 警告（>100 会溢出位图）。
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
                    | "icon_text_scale_migrated_v0318"
                    | "icon_bg_scale"
                    | "icon_font_tiers_enabled"
                    | "icon_font_tier_thresholds"
                    | "icon_font_tier_colors"
                    | "experimental_persist_last_known"
                    | "experimental_stats"
                    | "experimental_per_device_polling"
                    | "experimental_bluetooth_wired"
                    | "experimental_migrated_v0321"
                    | "ui_fine_tune_expanded"
                    | "ui_default_on_notice_v0322",
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
        // v0.3.18（U-1）默认值迁移：旧配置文件（无迁移标记）且文本大小
        // 恰为旧默认 100 → 视为「从未调整过」，随新默认迁到 140 并留警告
        // （显式写 100 的少数情形会被误迁，可接受，已在发布说明披露——
        // 用户调回 100 后标记已置位，不会再迁）。未含该键的旧配置则由
        // 新默认值自然生效（Default 即 140，无需警告）。标记恒置 true
        // 并随 to_toml 写出：后续启动不再迁移；旧版本读取时按未知键
        // 忽略 + 警告（降级兼容）。
        if !saw_text_scale_marker {
            if cfg.icon_text_scale == 100 {
                cfg.icon_text_scale = ICON_TEXT_SCALE_DEFAULT;
                warnings.push(ConfigWarning {
                    key: "icon_text_scale".into(),
                    reason: "migrated (v0.3.18): default text scale 100 -> 140; \
                             adjust in settings if 100 was intentional"
                        .into(),
                });
            }
            cfg.icon_text_scale_migrated_v0318 = true;
        }
        // v0.3.21（相位二冲突三裁决）：实验功能默认态改造——默认关仅对
        // 新装生效。存量配置（无本标记，v0.3.20 及更早）四个
        // experimental_* 恒迁 true 保留原行为（v0.3.16 起无条件开启），
        // 避免「升级后设备从托盘静默消失」的行为突变（F-A-2 口径；
        // 裁决仅立于「变更不静默」原则——安全性未知、待转正评审逐项
        // 证实，不采「零事故」作默认态依据，D 核对单差异 1）。
        // C-09：迁移只补缺失键、不覆盖显式解析到的键——「回退→再升级」
        // （旧版重写丢弃标记，用户按 README 手写补回显式 false）与
        // 「手写配置视为存量」路径下，显式 false 必须保留。
        if !saw_experimental_marker {
            if !saw_experimental_persist {
                cfg.experimental_persist_last_known = true;
            }
            if !saw_experimental_stats {
                cfg.experimental_stats = true;
            }
            if !saw_experimental_per_device {
                cfg.experimental_per_device_polling = true;
            }
            if !saw_experimental_bluetooth {
                cfg.experimental_bluetooth_wired = true;
            }
            warnings.push(ConfigWarning {
                key: "experimental_migrated_v0321".into(),
                reason: "migrated (v0.3.21): missing experimental_* keys filled ON for existing \
                         configs (explicit values preserved); new installs default ON (v0.3.22)"
                    .into(),
            });
            cfg.experimental_migrated_v0321 = true;
        }
        // v0.3.22（D-86）：记录本轮文件是否显式含 v0321 标记——Q3 一次性
        // 提示只面向「标记在」的存量（v0.3.21 期间新装/保存过设置）；
        // 无标记（v0.3.20 及更早直升级）本轮已补迁，不提示。
        // 运行时推导字段：不序列化、不入 to_toml、不从文件解析。
        cfg.experimental_marker_in_file = saw_experimental_marker;
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
             # 轮询间隔档位（秒）：300 / 600 / 1800 / 3600 / 7200（5/10/30/60/120 分钟），默认 1800\n\
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
             icon_text_scale_migrated_v0318 = {}\n\
             icon_bg_scale = {}\n\
             # 实验室（试验功能）：v0.3.22 起新装默认开（D-85）；显式值（含 false）保留（C-09）。\n\
             # 尚未完成全量真机验证——可到 设置→精细化设置 逐项关闭；保存后重启生效。\n\
             experimental_persist_last_known = {}\n\
             experimental_stats = {}\n\
             experimental_per_device_polling = {}\n\
             experimental_bluetooth_wired = {}\n\
             experimental_migrated_v0321 = {}\n\
             # 精细化设置闸门展开态（v0.3.22，D-87）：收起=false 仅隐藏选项，不影响使能。\n\
             ui_fine_tune_expanded = {}\n\
             # Q3 一次性提示闩锁（v0.3.22，D-86）：提示后取走置 false，不参与迁移逻辑。\n\
             ui_default_on_notice_v0322 = {}\n",
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
            self.icon_text_scale_migrated_v0318,
            self.icon_bg_scale,
            self.experimental_persist_last_known,
            self.experimental_stats,
            self.experimental_per_device_polling,
            self.experimental_bluetooth_wired,
            self.experimental_migrated_v0321,
            self.ui_fine_tune_expanded,
            self.ui_default_on_notice_v0322,
        )
    }

    /// v0.3.22（D-85/D-87）：实验室四键中已开启的计数（N/4，闸门文案
    /// 「精细化设置（试验功能，已开启 N/4 项）」与 Q3 toast 共用）。
    pub fn experimental_enabled_count(&self) -> usize {
        usize::from(self.experimental_persist_last_known)
            + usize::from(self.experimental_stats)
            + usize::from(self.experimental_per_device_polling)
            + usize::from(self.experimental_bluetooth_wired)
    }

    /// v0.3.22（D-86，终审采 D 差异 1）：Q3 一次性提示判定——v0321 标记
    /// 在文件中显式存在 ∧ 四键至少一键关 ∧ 提示闩锁未取走。无标记
    /// （v0.3.20 及更早直升级，本轮 parse 已补迁）不触发；四键全开
    /// 不触发；闩锁取走后不再触发（一次性）。
    pub fn default_on_notice_due(&self) -> bool {
        self.experimental_marker_in_file
            && self.ui_default_on_notice_v0322
            && self.experimental_enabled_count() < 4
    }

    /// 取走 Q3 提示闩锁（置 false；调用方负责持久化——无论本轮是否
    /// 实际弹提示，首个 v0.3.22 启动都应取走，避免「日后再关某项才
    /// 补弹」的迟到提示）。返回闩锁原值。
    pub fn take_default_on_notice(&mut self) -> bool {
        let was = self.ui_default_on_notice_v0322;
        self.ui_default_on_notice_v0322 = false;
        was
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
        assert_eq!(c.poll_interval_secs, 1800);
        assert_eq!(c.icon_style, IconStyle::Auto);
        assert!(c.notifications_enabled);
        assert!(!c.notification_silent);
        assert_eq!(c.icon_shape, IconShape::Rounded);
        assert_eq!(c.icon_background, IconBackground::Dark);
        assert_eq!(c.icon_font_color, IconFontColor::Auto);
        assert_eq!(c.icon_font_family, "Segoe UI");
        assert_eq!(c.low_threshold, 20);
        assert!(c.blink_enabled);
        // v0.3.18（U-1）：文本大小默认 140（原 100）；背景缩放默认 100%。
        assert_eq!(c.icon_text_scale, ICON_TEXT_SCALE_DEFAULT);
        assert_eq!(c.icon_text_scale, 140);
        // 默认值自带迁移标记（不触发迁移）。
        assert!(c.icon_text_scale_migrated_v0318);
        assert_eq!(c.icon_bg_scale, 100);
    }

    #[test]
    fn parse_full_document() {
        let (c, w) = Config::parse(
            "autostart = true\npoll_interval_secs = 300\nicon_style = \"dark\"\n\
             notifications_enabled = false\nnotification_silent = true\n\
             icon_shape = \"square\"\nicon_background = \"#0A6CFF\"\n\
             icon_font_color = \"#FFD21F\"\nicon_font_family = \"Consolas\"\n\
             low_threshold = 30\nblink_enabled = false\nexperimental_migrated_v0321 = true\n",
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
        let (c, w) = Config::parse(
            "evil_key = true\nlow_threshold = 30\nexperimental_migrated_v0321 = true\n",
        );
        assert_eq!(c.low_threshold, 30);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].key, "evil_key");
    }

    #[test]
    fn invalid_values_fall_back_with_warning() {
        let (c, w) = Config::parse(
            "icon_style = \"neon\"\nicon_shape = \"hex\"\nexperimental_migrated_v0321 = true\n",
        );
        assert_eq!(c.icon_style, IconStyle::Auto);
        assert_eq!(c.icon_shape, IconShape::Rounded);
        assert_eq!(w.len(), 2);
    }

    #[test]
    fn out_of_range_values_clamp_with_warning() {
        // M10/M12：越界钳制到边界 + 警告（不重置默认）。
        let (c, w) = Config::parse(
            "poll_interval_secs = 5\nlow_threshold = 80\nicon_font_family = \"  \"\n\
             experimental_migrated_v0321 = true\n",
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
        let (c, w) = Config::parse(
            "low_threshold_warn = 25\nlow_threshold_critical = 10\n\
             experimental_migrated_v0321 = true\n",
        );
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
        let (c, w) = Config::parse(
            "icon_background = \"transparent\"\nexperimental_migrated_v0321 = true\n",
        );
        assert_eq!(c.icon_shape, IconShape::Transparent);
        // 背景保默认 dark（透明时不渲染背景）。
        assert_eq!(c.icon_background, IconBackground::Dark);
        assert_eq!(w.len(), 1);
    }

    #[test]
    fn icon_color_options_parse_and_fall_back() {
        let (c, w) = Config::parse(
            "icon_background = \"light\"\nicon_font_color = \"auto\"\n\
             experimental_migrated_v0321 = true\n",
        );
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c.icon_background, IconBackground::Light);
        assert_eq!(c.icon_font_color, IconFontColor::Auto);

        let (c, w) = Config::parse(
            "icon_background = \"midnight\"\nicon_font_color = \"#12345\"\n\
             experimental_migrated_v0321 = true\n",
        );
        assert_eq!(c.icon_background, IconBackground::Dark);
        assert_eq!(c.icon_font_color, IconFontColor::Auto);
        assert_eq!(w.len(), 2);

        let (c, w) =
            Config::parse("icon_font_color = \"#ff00aa\"\nexperimental_migrated_v0321 = true\n");
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
        let (c, w) = Config::parse(
            "# comment\n\nlow_threshold = 15 # inline\n[section]\ngarbage line\n\
             experimental_migrated_v0321 = true\n",
        );
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

    /// D-66：轮询档位枚举固定为 300/600/1800/3600/7200，默认档 1800 在档位内，
    /// 且钳制边界与档位首末一致（防边界与档位表脱节）。
    #[test]
    fn poll_tiers_enum_and_default() {
        assert_eq!(POLL_TIERS_SECS, [300, 600, 1800, 3600, 7200]);
        assert_eq!(DEFAULT_POLL_SECS, 1800);
        assert!(POLL_TIERS_SECS.contains(&DEFAULT_POLL_SECS));
        assert_eq!(POLL_MIN_SECS, POLL_TIERS_SECS[0]);
        assert_eq!(POLL_MAX_SECS, POLL_TIERS_SECS[POLL_TIERS_SECS.len() - 1]);
        // 默认值即 Config::default() 的轮询间隔。
        assert_eq!(Config::default().poll_interval_secs, 1800);
    }

    /// D-66：轮询间隔档位归一化——旧档位与任意值吸附最近档（平局取低）。
    #[test]
    fn poll_secs_snap_to_tiers() {
        // 档位原值不动。
        for t in POLL_TIERS_SECS {
            assert_eq!(normalize_poll_secs(t), t);
        }
        // 被移除的旧档位：60 → 300，900 → 600；更早移除的 15/30/120 → 300。
        assert_eq!(normalize_poll_secs(60), 300);
        assert_eq!(normalize_poll_secs(900), 600);
        assert_eq!(normalize_poll_secs(15), 300);
        assert_eq!(normalize_poll_secs(30), 300);
        assert_eq!(normalize_poll_secs(120), 300);
        // 越界钳制：5 → 300，9999 → 7200。
        assert_eq!(normalize_poll_secs(5), POLL_MIN_SECS);
        assert_eq!(normalize_poll_secs(9999), POLL_MAX_SECS);
        // 非档位值吸附最近档：200 → 300（越界钳制）；700 → 600；2400 → 1800。
        assert_eq!(normalize_poll_secs(200), 300);
        assert_eq!(normalize_poll_secs(700), 600);
        assert_eq!(normalize_poll_secs(2400), 1800);
        // 平局取更低档：1200 与 600/1800 各差 600 → 取 600。
        assert_eq!(normalize_poll_secs(1200), 600);
        // 解析层：越界与非档位值都产生警告并落位。
        let (c, w) =
            Config::parse("poll_interval_secs = 900\nexperimental_migrated_v0321 = true\n");
        assert_eq!(c.poll_interval_secs, 600);
        assert_eq!(w.len(), 1);
        assert!(w[0].reason.contains("snapped"), "{w:?}");
        let (c, w) = Config::parse("poll_interval_secs = 5\nexperimental_migrated_v0321 = true\n");
        assert_eq!(c.poll_interval_secs, POLL_MIN_SECS);
        assert_eq!(w.len(), 1);
        assert!(w[0].reason.contains("out of range"), "{w:?}");
        // 档位值无警告。
        let (c, w) =
            Config::parse("poll_interval_secs = 1800\nexperimental_migrated_v0321 = true\n");
        assert_eq!(c.poll_interval_secs, 1800);
        assert!(w.is_empty(), "{w:?}");
    }

    /// D-66：存量配置迁移映射——已移除档位 60 → 300、900 → 600（取最近新档），
    /// 迁移各写一条警告（沿用 warnings 入日志机制）；合法旧值 300/600/1800
    /// 原样保留且无警告。
    #[test]
    fn poll_legacy_tiers_migrate_with_warning() {
        for (old, new) in [(60, 300), (900, 600)] {
            let (c, w) = Config::parse(&format!(
                "poll_interval_secs = {old}\nexperimental_migrated_v0321 = true\n"
            ));
            assert_eq!(c.poll_interval_secs, new, "{old} should migrate to {new}");
            assert_eq!(w.len(), 1, "{w:?}");
            assert_eq!(w[0].key, "poll_interval_secs");
        }
        for keep in [300, 600, 1800] {
            let (c, w) = Config::parse(&format!(
                "poll_interval_secs = {keep}\nexperimental_migrated_v0321 = true\n"
            ));
            assert_eq!(c.poll_interval_secs, keep);
            assert!(w.is_empty(), "{keep} is a valid tier: {w:?}");
        }
    }

    /// D-48：文本/背景缩放键——解析、钳制、往返。
    #[test]
    fn icon_scale_keys_parse_clamp_roundtrip() {
        let (c, w) = Config::parse(
            "icon_text_scale = 130\nicon_bg_scale = 80\nexperimental_migrated_v0321 = true\n",
        );
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c.icon_text_scale, 130);
        assert_eq!(c.icon_bg_scale, 80);
        let (back, w2) = Config::parse(&c.to_toml());
        assert!(w2.is_empty(), "{w2:?}");
        assert_eq!(back, c);

        // 越界钳制 + 警告：文本 200 → 160、10 → 40；背景 130 → 100（几何上限）、
        // 10 → 20（D-57 新边界）。
        let (c, w) = Config::parse(
            "icon_text_scale = 200\nicon_bg_scale = 130\nexperimental_migrated_v0321 = true\n",
        );
        assert_eq!(c.icon_text_scale, ICON_TEXT_SCALE_MAX);
        assert_eq!(c.icon_bg_scale, ICON_BG_SCALE_MAX);
        assert_eq!(w.len(), 2);
        assert!(w.iter().any(|x| x.key == "icon_text_scale"));
        assert!(w.iter().any(|x| x.key == "icon_bg_scale"));
        let (c, w) = Config::parse("icon_text_scale = 10\nexperimental_migrated_v0321 = true\n");
        assert_eq!(c.icon_text_scale, ICON_TEXT_SCALE_MIN);
        assert_eq!(w.len(), 1);
        let (c, w) = Config::parse("icon_bg_scale = 10\nexperimental_migrated_v0321 = true\n");
        assert_eq!(c.icon_bg_scale, ICON_BG_SCALE_MIN);
        assert_eq!(w.len(), 1);
        // 非整数 / 布尔 → 默认保留 + 警告（v0.3.18：默认 140）。
        let (c, w) = Config::parse("icon_text_scale = true\nexperimental_migrated_v0321 = true\n");
        assert_eq!(c.icon_text_scale, ICON_TEXT_SCALE_DEFAULT);
        assert_eq!(w.len(), 1);
    }

    /// v0.3.18（U-1）：默认值 100→140 的迁移语义——无标记且恰为旧默认
    /// 100 才迁移；显式其他值不动；已有标记不重复迁移；迁移结果可
    /// roundtrip（标记写出后再次解析不再迁移、无警告）。
    #[test]
    fn icon_text_scale_v0318_default_migration() {
        // 无标记 + 恰为旧默认 100 → 迁 140 + 警告 + 标记置位。
        let (c, w) = Config::parse("icon_text_scale = 100\n");
        assert_eq!(c.icon_text_scale, ICON_TEXT_SCALE_DEFAULT);
        assert!(c.icon_text_scale_migrated_v0318);
        assert!(
            w.iter()
                .any(|x| x.key == "icon_text_scale" && x.reason.contains("v0.3.18")),
            "{w:?}"
        );
        // 无标记 + 显式非默认值 → 保持不变、不警告、标记置位。
        let (c, w) = Config::parse("icon_text_scale = 130\n");
        assert_eq!(c.icon_text_scale, 130);
        assert!(c.icon_text_scale_migrated_v0318);
        assert!(!w.iter().any(|x| x.key == "icon_text_scale"), "{w:?}");
        // 已有标记 + 显式 100 → 不迁移（新版本上主动调回 100 受尊重）。
        let (c, w) =
            Config::parse("icon_text_scale = 100\nicon_text_scale_migrated_v0318 = true\n");
        assert_eq!(c.icon_text_scale, 100);
        assert!(!w.iter().any(|x| x.key == "icon_text_scale"), "{w:?}");
        // roundtrip：迁移结果序列化后再解析，值与标记稳定、零警告。
        let (c1, _) = Config::parse("icon_text_scale = 100\n");
        let (c2, w2) = Config::parse(&c1.to_toml());
        // experimental_marker_in_file 是解析期派生元数据（本轮输入
        // 文件是否出现 v0321 标记），不参与持久化等价：to_toml 全量
        // 物化后标记必然出现，故以序列化形式与关键字段比较等价性。
        assert_eq!(c1.to_toml(), c2.to_toml());
        assert_eq!(c1.icon_text_scale, c2.icon_text_scale);
        assert_eq!(
            c1.icon_text_scale_migrated_v0318,
            c2.icon_text_scale_migrated_v0318
        );
        assert!(!w2.iter().any(|x| x.key == "icon_text_scale"), "{w2:?}");
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
             icon_font_tier_colors = [\"#E81123\", \"#FFD21F\", \"#00CC6A\", \"#0078D7\"]\n\
             experimental_migrated_v0321 = true\n",
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
             icon_font_tier_colors = [\"#12345\", \"#E81123\"]\n\
             experimental_migrated_v0321 = true\n",
        );
        assert_eq!(c.icon_font_tier_thresholds, vec![1, 99]);
        assert_eq!(c.icon_font_tier_colors.len(), 3);
        assert!(w.iter().any(|x| x.key == "icon_font_tier_thresholds"));
        assert!(w.iter().any(|x| x.key == "icon_font_tier_colors"));
        // 数组元素类型错误 → 整组保默认 + 警告。
        let (c, w) = Config::parse(
            "icon_font_tier_thresholds = [20, \"50\"]\nexperimental_migrated_v0321 = true\n",
        );
        assert_eq!(c.icon_font_tier_thresholds, vec![20, 50]);
        assert_eq!(w.len(), 1);
        // 数组值给到标量键 → catch-all 警告；v0.3.5 默认开，「保默认」即开。
        let (c, w) =
            Config::parse("icon_font_tiers_enabled = [true]\nexperimental_migrated_v0321 = true\n");
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
        let (c, w) = Config::parse(
            "icon_font_tiers_enabled = false\nicon_font_color = \"#FF0000\"\n\
             experimental_migrated_v0321 = true\n",
        );
        assert!(!c.icon_font_tiers_enabled);
        assert!(w.is_empty(), "{w:?}");
        // 勾选开 → 无迁移警告（本来就在分档模式）。
        let (c, w) =
            Config::parse("icon_font_tiers_enabled = true\nexperimental_migrated_v0321 = true\n");
        assert!(c.icon_font_tiers_enabled);
        assert!(w.is_empty(), "{w:?}");
        // 空文件按存量语义处理（新装配置文件不存在、走 Config::default()
        // 不经 parse）：分档开；仅触发 experimental 迁移一条警告。
        let (c, w) = Config::parse("");
        assert!(c.icon_font_tiers_enabled);
        assert_eq!(w.len(), 1, "{w:?}");
        assert_eq!(w[0].key, "experimental_migrated_v0321");
    }

    /// D-85（v0.3.22）：实验功能键——新装（配置文件不存在，直接走
    /// `Config::default()`、不经 parse）默认全开、标记恒写；默认值首轮
    /// 写盘后再解析即稳态（标记在 → 零警告）。
    #[test]
    fn experimental_keys_default_on_for_new_install() {
        let c = Config::default();
        assert!(c.experimental_persist_last_known);
        assert!(c.experimental_stats);
        assert!(c.experimental_per_device_polling);
        assert!(c.experimental_bluetooth_wired);
        // 新装视为已迁移（不落存量迁移路径）。
        assert!(c.experimental_migrated_v0321);
        // 新装四键全开 → Q3 提示不触发（D-86）。
        assert!(!c.default_on_notice_due());
        let (back, w) = Config::parse(&c.to_toml());
        assert_eq!(back, c);
        assert!(w.is_empty(), "{w:?}");
        // 非法值回退默认（开）+ 键名警告。
        let (c2, w2) =
            Config::parse("experimental_stats = \"nope\"\nexperimental_migrated_v0321 = true\n");
        assert!(c2.experimental_stats);
        assert!(w2.iter().any(|x| x.key == "experimental_stats"), "{w2:?}");
    }

    /// 相位二（第 4 项）：存量配置（无标记键）→ 四键恒迁 true + 迁移警告
    /// + 标记置位（变更不静默）；迁完幂等；显式写过的键值不被覆盖。
    #[test]
    fn experimental_keys_migrate_on_for_existing_config() {
        let (c, w) = Config::parse("low_threshold = 25\n");
        assert!(c.experimental_persist_last_known);
        assert!(c.experimental_stats);
        assert!(c.experimental_per_device_polling);
        assert!(c.experimental_bluetooth_wired);
        assert!(c.experimental_migrated_v0321);
        assert!(
            w.iter()
                .any(|x| x.key == "experimental_migrated_v0321" && x.reason.contains("migrated")),
            "{w:?}"
        );
        // 幂等：迁完写出再读回，不再产生迁移警告，四键保持 true。
        let (c2, w2) = Config::parse(&c.to_toml());
        assert!(c2.experimental_migrated_v0321);
        assert!(c2.experimental_stats);
        assert!(
            !w2.iter().any(|x| x.key == "experimental_migrated_v0321"),
            "{w2:?}"
        );
        // 显式 false + 标记 → 保留（用户明确关过的不再被迁移打开）。
        let (c3, w3) =
            Config::parse("experimental_migrated_v0321 = true\nexperimental_stats = false\n");
        assert!(!c3.experimental_stats);
        // D-85（v0.3.22）：标记在时缺失键不走迁移、随新默认 true 生效
        // （v0.3.21 下为 false；手写含标记的部分键配置属此口径，已在
        // RELEASES 误伤面披露——如需关闭请显式写 false）。
        assert!(c3.experimental_persist_last_known);
        assert!(
            !w3.iter().any(|x| x.key == "experimental_migrated_v0321"),
            "{w3:?}"
        );
    }

    /// C-09：迁移只补缺失键、不覆盖显式解析到的键——
    /// 「回退→再升级」与「手写配置视为存量」路径：v0.3.21 显式 false +
    /// 标记 → 旧版（v0.3.20）键集重写丢弃标记（用户对被丢弃的
    /// experimental_* 键按 README 指引手写补回显式 false）→ 再升级
    /// parse：无标记触发迁移，但显式 false 必须保留，仅缺失键补 true。
    #[test]
    fn experimental_migration_preserves_explicit_false_without_marker() {
        // 模拟旧版键集重写后的文件：标记被丢弃，用户手写补回两个显式
        // false，其余两个 experimental_* 键缺失。
        let (c, w) = Config::parse(
            "low_threshold = 25\nexperimental_stats = false\nexperimental_bluetooth_wired = false\n",
        );
        // 显式 false 保留（修复前被无条件重置为 true——本断言红）。
        assert!(!c.experimental_stats);
        assert!(!c.experimental_bluetooth_wired);
        // 缺失键按存量口径补 true。
        assert!(c.experimental_persist_last_known);
        assert!(c.experimental_per_device_polling);
        // 标记置位 + 迁移警告（变更不静默）。
        assert!(c.experimental_migrated_v0321);
        assert!(
            w.iter()
                .any(|x| x.key == "experimental_migrated_v0321" && x.reason.contains("migrated")),
            "{w:?}"
        );
        // 迁完幂等：写出再读回，显式 false 仍在、不再产生迁移警告。
        let (c2, w2) = Config::parse(&c.to_toml());
        assert!(!c2.experimental_stats);
        assert!(!c2.experimental_bluetooth_wired);
        assert!(
            !w2.iter().any(|x| x.key == "experimental_migrated_v0321"),
            "{w2:?}"
        );
    }

    /// D-85：to_toml 恒写实验/UI 辅助 7 键，round-trip 等价。
    #[test]
    fn experimental_keys_toml_roundtrip() {
        let c = Config {
            experimental_persist_last_known: true,
            experimental_stats: true,
            experimental_per_device_polling: true,
            experimental_bluetooth_wired: false,
            ui_fine_tune_expanded: true,
            ui_default_on_notice_v0322: false,
            ..Config::default()
        };
        let (back, w) = Config::parse(&c.to_toml());
        assert_eq!(back, c);
        assert!(
            !w.iter().any(|x| x.key.starts_with("experimental")),
            "{w:?}"
        );
        assert!(!w.iter().any(|x| x.key.starts_with("ui_")), "{w:?}");
    }

    /// D-86/D-87：Q3 一次性提示三态触发 + 闩锁一次性（取走即消）。
    #[test]
    fn default_on_notice_three_states_and_latch() {
        // ① 标记在 ∧ 至少一键 false（Q3：v0.3.21 期间新装/保存过设置）
        // → 触发，文案计数 N/4 正确。
        let (mut c, _) = Config::parse(
            "experimental_migrated_v0321 = true\nexperimental_persist_last_known = false\n\
             experimental_stats = false\nexperimental_per_device_polling = false\n\
             experimental_bluetooth_wired = false\n",
        );
        assert!(c.default_on_notice_due());
        assert_eq!(c.experimental_enabled_count(), 0);
        // ② 四键全 true → 不触发。
        let (c_all, _) = Config::parse(
            "experimental_migrated_v0321 = true\nexperimental_persist_last_known = true\n\
             experimental_stats = true\nexperimental_per_device_polling = true\n\
             experimental_bluetooth_wired = true\n",
        );
        assert!(!c_all.default_on_notice_due());
        assert_eq!(c_all.experimental_enabled_count(), 4);
        // ③ 无标记（v0.3.20 及更早直升级；含显式 false 的 C-09 路径）
        // → 不触发（本轮 parse 已补迁，提示只面向「标记在」的存量）。
        let (c_nomark, _) = Config::parse("low_threshold = 25\nexperimental_stats = false\n");
        assert!(!c_nomark.experimental_marker_in_file);
        assert!(!c_nomark.default_on_notice_due());
        // ④ 闩锁一次性：取走并物化后再读 → 不触发（二次启动不重复）。
        assert!(c.take_default_on_notice());
        assert!(!c.default_on_notice_due());
        let (mut c2, _) = Config::parse(&c.to_toml());
        assert!(!c2.ui_default_on_notice_v0322);
        assert!(!c2.default_on_notice_due());
        // take 幂等：闩锁已取走时再取返回 false。
        assert!(!c2.take_default_on_notice());
    }

    /// D-87（测试清单「闸门收起不改使能」配置层断言）：闸门展开态
    /// 翻转不改变四键使能——同一份配置 ± ui_fine_tune_expanded，四键
    /// 逐键相等。
    #[test]
    fn gate_expansion_does_not_change_enablement() {
        let base = "experimental_migrated_v0321 = true\nexperimental_persist_last_known = true\n\
             experimental_stats = false\nexperimental_per_device_polling = true\n\
             experimental_bluetooth_wired = false\n";
        let (collapsed, _) = Config::parse(base);
        let (expanded, _) = Config::parse(&format!("{base}ui_fine_tune_expanded = true\n"));
        assert!(!collapsed.ui_fine_tune_expanded);
        assert!(expanded.ui_fine_tune_expanded);
        assert_eq!(
            collapsed.experimental_enabled_count(),
            expanded.experimental_enabled_count()
        );
        assert_eq!(
            collapsed.experimental_persist_last_known,
            expanded.experimental_persist_last_known
        );
        assert_eq!(collapsed.experimental_stats, expanded.experimental_stats);
        assert_eq!(
            collapsed.experimental_per_device_polling,
            expanded.experimental_per_device_polling
        );
        assert_eq!(
            collapsed.experimental_bluetooth_wired,
            expanded.experimental_bluetooth_wired
        );
    }

    /// D-86/D-87（测试清单「GUI 保存只写显式值」配置层断言）：UI 辅助
    /// 键保存不隐式改写任何实验键——仅显式改动的键发生变化。
    #[test]
    fn ui_key_save_does_not_touch_experimental_keys() {
        let (mut c, _) = Config::parse(
            "experimental_migrated_v0321 = true\nexperimental_persist_last_known = true\n\
             experimental_stats = false\nexperimental_per_device_polling = true\n\
             experimental_bluetooth_wired = false\n",
        );
        let before = (
            c.experimental_persist_last_known,
            c.experimental_stats,
            c.experimental_per_device_polling,
            c.experimental_bluetooth_wired,
        );
        // 模拟设置窗口只改闸门展开态并物化保存。
        c.ui_fine_tune_expanded = true;
        let (back, _) = Config::parse(&c.to_toml());
        let after = (
            back.experimental_persist_last_known,
            back.experimental_stats,
            back.experimental_per_device_polling,
            back.experimental_bluetooth_wired,
        );
        assert_eq!(before, after);
        assert!(back.ui_fine_tune_expanded);
        // 物化文件含 v0321 标记 → 再解析 marker_in_file 为 true
        // （Q3 判定输入正确传递）。
        assert!(back.experimental_marker_in_file);
    }
}
