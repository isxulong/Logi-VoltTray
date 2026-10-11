//! 配置 schema（评审 P1-8：7 个键）+ 容错解析 + mtime 热重载。
//!
//! 配置文件：`%APPDATA%\LogiVoltTray\config.toml`（扁平键值，无 section）。
//! 解析器为扁平 TOML 子集（bool / 整数 / 字符串），见 docs/decisions.md D-07。
//! 容错规则：未知键 / 非法值 / 越界值 → 回退默认 + 诊断警告，绝不 panic、绝不拒绝启动。

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

/// 配置（P1-8：7 键）。
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
        let mut warn = |key: &str, reason: &str| warnings.push(ConfigWarning { key: key.into(), reason: reason.into() });

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
                ("low_threshold_warn", Value::Int(n)) if *n <= 100 => cfg.low_threshold_warn = *n as u8,
                ("low_threshold_critical", Value::Int(n)) if *n <= 100 => cfg.low_threshold_critical = *n as u8,
                ("poll_interval_secs", Value::Int(n)) if (POLL_MIN_SECS..=POLL_MAX_SECS).contains(n) => {
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
                (
                    "autostart"
                    | "low_threshold_warn"
                    | "low_threshold_critical"
                    | "poll_interval_secs"
                    | "icon_style"
                    | "notifications_enabled"
                    | "notification_silent",
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
             notification_silent = {}\n",
            self.autostart,
            self.low_threshold_warn,
            self.low_threshold_critical,
            self.poll_interval_secs,
            self.icon_style.name(),
            self.notifications_enabled,
            self.notification_silent,
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
        let mtime = std::fs::metadata(&self.path).and_then(|m| m.modified()).ok();
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
    }

    #[test]
    fn parse_full_document() {
        let (c, w) = Config::parse(
            "autostart = true\nlow_threshold_warn = 25\nlow_threshold_critical = 5\n\
             poll_interval_secs = 30\nicon_style = \"dark\"\nnotifications_enabled = false\n\
             notification_silent = true\n",
        );
        assert!(w.is_empty(), "{w:?}");
        assert!(c.autostart);
        assert_eq!(c.low_threshold_warn, 25);
        assert_eq!(c.low_threshold_critical, 5);
        assert_eq!(c.poll_interval_secs, 30);
        assert_eq!(c.icon_style, IconStyle::Dark);
        assert!(!c.notifications_enabled);
        assert!(c.notification_silent);
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
        let (c, w) = Config::parse("low_threshold_warn = 999\npoll_interval_secs = 5\nicon_style = \"neon\"\n");
        assert_eq!(c.low_threshold_warn, DEFAULT_WARN);
        assert_eq!(c.poll_interval_secs, DEFAULT_POLL_SECS);
        assert_eq!(c.icon_style, IconStyle::Auto);
        assert_eq!(w.len(), 3);
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
        let (c, w) = Config::parse("# comment\n\nlow_threshold_warn = 15 # inline\n[section]\ngarbage line\n");
        assert_eq!(c.low_threshold_warn, 15);
        assert_eq!(w.len(), 2); // section + garbage
    }

    #[test]
    fn toml_roundtrip() {
        let c = Config { autostart: true, low_threshold_warn: 22, ..Config::default() };
        let (back, w) = Config::parse(&c.to_toml());
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(back, c);
    }
}
