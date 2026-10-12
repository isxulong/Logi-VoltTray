//! 配置 schema（评审 P1-8：7 个键 + v0.2.0 可读性新增 2 键）+ 容错解析 + mtime 热重载。
//!
//! 配置文件：`%APPDATA%\LogiVoltTray\config.toml`（扁平键值，无 section）。
//! 解析器为扁平 TOML 子集（bool / 整数 / 字符串），见 docs/decisions.md D-07。
//! 容错规则：未知键 / 非法值 / 越界值 → 回退默认 + 诊断警告，绝不 panic、绝不拒绝启动。
//!
//! 键位一览（v0.2.0，共 9 键）：
//! | 键 | 类型 | 默认 | 取值范围 / 说明 |
//! | --- | --- | --- | --- |
//! | `autostart` | bool | `false` | 开机自启（写入启动快捷方式） |
//! | `low_threshold_warn` | int | `20` | 低电量提醒阈值（%），需 > critical |
//! | `low_threshold_critical` | int | `10` | 严重低电量阈值（%），需 < warn |
//! | `poll_interval_secs` | int | `60` | `15`–`300`，轮询间隔（秒） |
//! | `icon_style` | str | `"auto"` | `auto` / `light` / `dark`，图标整体风格 |
//! | `notifications_enabled` | bool | `true` | 低电量通知开关 |
//! | `notification_silent` | bool | `false` | 通知静默（无提示音） |
//! | `icon_background` | str | `"dark"` | `transparent` / `light` / `dark`，托盘数字底色（v0.2.0） |
//! | `icon_font_color` | str | `"auto"` | `"auto"`（按底色自动高对比）或 `"#RRGGBB"`（v0.2.0） |
//!
//! v0.2.0 可读性默认组合 = 深色圆角底 + 自动白字：深色与浅色任务栏上均清晰可读
//! （不再默认无背景黑字）；两项均参与热重载，改配置即时生效（重建图标缓存）。

use std::fmt;
use std::io;
use std::path::PathBuf;
use std::time::SystemTime;

pub const DEFAULT_WARN: u8 = 20;
pub const DEFAULT_CRITICAL: u8 = 10;
pub const DEFAULT_POLL_SECS: u64 = 60;
pub const POLL_MIN_SECS: u64 = 15;
pub const POLL_MAX_SECS: u64 = 300;

/// 图标配色策略。
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

/// 托盘图标背景设置（v0.2.0 可读性：真机反馈"无背景黑字不清晰"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IconBackground {
    /// 完全透明，仅渲染数字（旧版行为，浅色任务栏上黑字可读时可用）。
    Transparent,
    /// 浅色圆角底（近似白），配深色数字。
    Light,
    /// 深色圆角底（近似黑），配浅色数字——默认：深浅两种任务栏上都可读。
    Dark,
}

impl IconBackground {
    pub fn name(self) -> &'static str {
        match self {
            Self::Transparent => "transparent",
            Self::Light => "light",
            Self::Dark => "dark",
        }
    }

    /// 背景填充 RGB（Transparent 时渲染层不填充）。
    pub fn rgb(self) -> Option<(u8, u8, u8)> {
        match self {
            Self::Transparent => None,
            Self::Light => Some((0xF2, 0xF2, 0xF2)),
            Self::Dark => Some((0x1F, 0x1F, 0x1F)),
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

/// 电量字体颜色设置（v0.2.0 可读性）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum IconFontColor {
    /// 自动：按电量档位着色（正常/警告/严重），正常档颜色依背景取高对比。
    Auto,
    /// 固定色：覆盖正常/警告/严重档，未知(?)保持灰。
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

/// 配置（P1-8：7 键 + v0.2.0 新增 2 键）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Config {
    /// 开机自启（HKCU Run 单值）。
    pub autostart: bool,
    /// 低电提醒阈值（%），默认 20。
    pub low_threshold_warn: u8,
    /// 严重低电阈值（%），默认 10，必须 < warn。
    pub low_threshold_critical: u8,
    /// 轮询间隔（秒），默认 60，范围 15..=300。
    pub poll_interval_secs: u64,
    /// 图标配色：auto/light/dark。
    pub icon_style: IconStyle,
    /// 通知总开关。
    pub notifications_enabled: bool,
    /// 静默通知（无声音）。
    pub notification_silent: bool,
    /// 托盘图标背景：transparent/light/dark，默认 dark（深浅任务栏均可读）。
    pub icon_background: IconBackground,
    /// 电量字体颜色：auto（按电量档位）或 "#RRGGBB"，默认 auto。
    pub icon_font_color: IconFontColor,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            autostart: false,
            low_threshold_warn: DEFAULT_WARN,
            low_threshold_critical: DEFAULT_CRITICAL,
            poll_interval_secs: DEFAULT_POLL_SECS,
            icon_style: IconStyle::Auto,
            notifications_enabled: true,
            notification_silent: false,
            icon_background: IconBackground::Dark,
            icon_font_color: IconFontColor::Auto,
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
                ("low_threshold_warn", Value::Int(n)) if *n <= 100 => {
                    cfg.low_threshold_warn = *n as u8
                }
                ("low_threshold_critical", Value::Int(n)) if *n <= 100 => {
                    cfg.low_threshold_critical = *n as u8
                }
                ("poll_interval_secs", Value::Int(n))
                    if (POLL_MIN_SECS..=POLL_MAX_SECS).contains(n) =>
                {
                    cfg.poll_interval_secs = *n
                }
                ("icon_style", Value::Str(s)) => match s.as_str() {
                    "auto" => cfg.icon_style = IconStyle::Auto,
                    "light" => cfg.icon_style = IconStyle::Light,
                    "dark" => cfg.icon_style = IconStyle::Dark,
                    _ => warn(key, "unknown icon_style; default kept"),
                },
                ("notifications_enabled", Value::Bool(b)) => cfg.notifications_enabled = *b,
                ("notification_silent", Value::Bool(b)) => cfg.notification_silent = *b,
                ("icon_background", Value::Str(s)) => match s.as_str() {
                    "transparent" => cfg.icon_background = IconBackground::Transparent,
                    "light" => cfg.icon_background = IconBackground::Light,
                    "dark" => cfg.icon_background = IconBackground::Dark,
                    _ => warn(key, "unknown icon_background; default kept"),
                },
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
                (
                    "autostart"
                    | "low_threshold_warn"
                    | "low_threshold_critical"
                    | "poll_interval_secs"
                    | "icon_style"
                    | "notifications_enabled"
                    | "notification_silent"
                    | "icon_background"
                    | "icon_font_color",
                    _,
                ) => warn(key, "invalid value or out of range; default kept"),
                (other, _) => warn(other, "unknown key; ignored"),
            }
        }

        // 跨键校验：critical 必须严格小于 warn（评审 P1-8）。
        if cfg.low_threshold_critical >= cfg.low_threshold_warn {
            warn(
                "low_threshold_critical",
                "must be < low_threshold_warn; both reset to defaults",
            );
            cfg.low_threshold_warn = DEFAULT_WARN;
            cfg.low_threshold_critical = DEFAULT_CRITICAL;
        }
        (cfg, warnings)
    }

    /// 序列化为扁平 TOML（供首次运行生成默认配置）。
    pub fn to_toml(&self) -> String {
        format!(
            "# Logi VoltTray 配置（扁平键值；未知键会被忽略并产生诊断警告）\n\
             autostart = {}\n\
             low_threshold_warn = {}\n\
             low_threshold_critical = {}\n\
             poll_interval_secs = {}\n\
             icon_style = \"{}\"\n\
             notifications_enabled = {}\n\
             notification_silent = {}\n\
             icon_background = \"{}\"\n\
             icon_font_color = \"{}\"\n",
            self.autostart,
            self.low_threshold_warn,
            self.low_threshold_critical,
            self.poll_interval_secs,
            self.icon_style.name(),
            self.notifications_enabled,
            self.notification_silent,
            self.icon_background.name(),
            self.icon_font_color.name(),
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
    fn defaults_match_review_schema() {
        let c = Config::default();
        assert_eq!(c.low_threshold_warn, 20);
        assert_eq!(c.low_threshold_critical, 10);
        assert_eq!(c.poll_interval_secs, 60);
        assert_eq!(c.icon_style, IconStyle::Auto);
        assert!(c.notifications_enabled);
        assert!(!c.notification_silent);
        assert!(!c.autostart);
        assert_eq!(c.icon_background, IconBackground::Dark);
        assert_eq!(c.icon_font_color, IconFontColor::Auto);
    }

    #[test]
    fn parse_full_document() {
        let (c, w) = Config::parse(
            "autostart = true\nlow_threshold_warn = 25\nlow_threshold_critical = 5\n\
             poll_interval_secs = 30\nicon_style = \"dark\"\nnotifications_enabled = false\n\
             notification_silent = true\nicon_background = \"light\"\n\
             icon_font_color = \"#0A6CFF\"\n",
        );
        assert!(w.is_empty(), "{w:?}");
        assert!(c.autostart);
        assert_eq!(c.low_threshold_warn, 25);
        assert_eq!(c.low_threshold_critical, 5);
        assert_eq!(c.poll_interval_secs, 30);
        assert_eq!(c.icon_style, IconStyle::Dark);
        assert!(!c.notifications_enabled);
        assert!(c.notification_silent);
        assert_eq!(c.icon_background, IconBackground::Light);
        assert_eq!(
            c.icon_font_color,
            IconFontColor::Fixed(RgbColor {
                r: 0x0A,
                g: 0x6C,
                b: 0xFF
            })
        );
    }

    #[test]
    fn unknown_keys_warn_not_fail() {
        let (c, w) = Config::parse("evil_key = true\nlow_threshold_warn = 30\n");
        assert_eq!(c.low_threshold_warn, 30);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].key, "evil_key");
    }

    #[test]
    fn invalid_values_fall_back_with_warning() {
        let (c, w) = Config::parse(
            "low_threshold_warn = 999\npoll_interval_secs = 5\nicon_style = \"neon\"\n",
        );
        assert_eq!(c.low_threshold_warn, DEFAULT_WARN);
        assert_eq!(c.poll_interval_secs, DEFAULT_POLL_SECS);
        assert_eq!(c.icon_style, IconStyle::Auto);
        assert_eq!(w.len(), 3);
    }

    #[test]
    fn icon_readability_keys_parse_and_fall_back() {
        // 合法取值（大小写不敏感 hex、auto）。
        let (c, w) =
            Config::parse("icon_background = \"transparent\"\nicon_font_color = \"auto\"\n");
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c.icon_background, IconBackground::Transparent);
        assert_eq!(c.icon_font_color, IconFontColor::Auto);

        // 非法取值回退默认 + 警告。
        let (c, w) =
            Config::parse("icon_background = \"midnight\"\nicon_font_color = \"#12345\"\n");
        assert_eq!(c.icon_background, IconBackground::Dark);
        assert_eq!(c.icon_font_color, IconFontColor::Auto);
        assert_eq!(w.len(), 2);

        // 小写 hex 同样接受，roundtrip 后保持。
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
    fn critical_must_be_below_warn() {
        let (c, w) = Config::parse("low_threshold_warn = 10\nlow_threshold_critical = 20\n");
        assert_eq!(c.low_threshold_warn, DEFAULT_WARN);
        assert_eq!(c.low_threshold_critical, DEFAULT_CRITICAL);
        assert!(w.iter().any(|x| x.key == "low_threshold_critical"));
    }

    #[test]
    fn comments_and_malformed_lines_tolerated() {
        let (c, w) = Config::parse(
            "# comment\n\nlow_threshold_warn = 15 # inline\n[section]\ngarbage line\n",
        );
        assert_eq!(c.low_threshold_warn, 15);
        assert_eq!(w.len(), 2); // section + garbage
    }

    #[test]
    fn toml_roundtrip() {
        let c = Config {
            autostart: true,
            low_threshold_warn: 22,
            ..Config::default()
        };
        let (back, w) = Config::parse(&c.to_toml());
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(back, c);
    }
}
