//! 开机自启（写入边界 1：HKCU\...\Run 单值 "LogiVoltTray"）。
//! 非 Windows 平台提供同签名桩（返回 Unsupported），保证跨平台可编译。
//!
//! U-7（v0.3.19）：新增启动一次性对账 [`reconcile`]——注册表路径陈旧/
//! 缺失时按当前 exe 收口（四格状态矩阵全收口），解决「exe 改名换版后
//! 自启静默失效、注册表残留旧路径」的问题。路径比较/写入均为纯字符串
//! 规范化（无 I/O、无 Win32 依赖），跨平台可单测（C-3）。

pub const RUN_VALUE_NAME: &str = "LogiVoltTray";

// ============================================================================
// U-7（v0.3.19）路径规范化纯函数（C-2a）——全部置于 cfg(windows) 之外，
// 非 Windows 平台同样编译、单测随 CI 全平台可跑（C-3）。
// 设计原则：处理不了的形式一律保守落入「判不匹配 → 收敛重写为当前
// exe 的规范式」——重写目标恒为正在运行的实例（不可能写出死值），代价
// 至多是多写一次注册表，不会产生「实际不同却判等」的假阴性方向错误。
// 已知边界（有意划界，RELEASES 披露）：8.3 短名不展开（需 Win32，破坏
// 纯函数）；开启目录级大小写敏感的 WSL 生态目录下，仅大小写不同的两条
// 路径可能是两个文件——纯函数无法判定目录属性，登记为残余风险。
// ============================================================================

/// 剥除 Windows 长路径前缀 `\\?\` / `\\?\UNC\`（大小写不敏感）。
///
/// 写入侧 canonical 化（BF-1）：`std::env::current_exe` 底层的
/// GetModuleFileNameW 在长路径场景可返回 `\\?\` 形态，而该形态在开机
/// shell 启动链（Run 键解析）下常无法启动——写入与比较双侧都必须先
/// 剥前缀，保证 Run 值恒为标准 Win32 形态。
pub(crate) fn strip_verbose_prefixes(raw: &str) -> String {
    let lower = raw.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix("\\\\?\\unc\\") {
        // `\\?\UNC\server\share` → `\\server\share`（还原 UNC 前导双反斜杠）
        format!("\\\\{}", &raw[raw.len() - rest.len()..])
    } else if let Some(rest) = lower.strip_prefix("\\\\?\\") {
        raw[raw.len() - rest.len()..].to_string()
    } else {
        raw.to_string()
    }
}

/// Run 值路径段的规范化（R1~R7，全部纯字符串操作，按应用顺序）：
/// R1 去首尾空白；R2 剥一层成对首尾双引号（残缺引号保守保留——判
/// 不等 → 收敛重写）；R3 正斜杠转反斜杠；R4 折叠连续反斜杠（保留 UNC
/// 前导 `\\`）；R5 剥 `\\?\` / `\\?\UNC\` 前缀（大小写不敏感）；R6 剥
/// 尾部反斜杠（保留 `C:\` 根形式）；R7 全串小写（NTFS 默认大小写不敏感）。
pub(crate) fn normalize_path(raw: &str) -> String {
    let mut s: String = raw.trim().to_string();
    // R2：剥除一层成对首尾双引号（且仅一层）。
    let b = s.as_bytes();
    if b.len() >= 2 && b.first() == Some(&b'"') && b.last() == Some(&b'"') {
        s = s[1..s.len() - 1].to_string();
    }
    // R3：正斜杠统一为反斜杠。
    s = s.replace('/', "\\");
    // R4：折叠连续反斜杠；UNC 前导 `\\` 原样保留。
    let mut collapsed = String::with_capacity(s.len());
    let mut rest = s.as_str();
    if rest.starts_with("\\\\") {
        collapsed.push_str("\\\\");
        rest = &rest[2..];
    }
    let mut chars = rest.chars().peekable();
    while let Some(c) = chars.next() {
        collapsed.push(c);
        if c == '\\' {
            while chars.peek() == Some(&'\\') {
                chars.next();
            }
        }
    }
    s = collapsed;
    // R5：剥长路径前缀（在 R3/R4 之后判定，大小写不敏感）。
    s = strip_verbose_prefixes(&s);
    // R6：剥尾部反斜杠（整串长度 >3 才剥，保留 `C:\` 根形式）。
    while s.len() > 3 && s.ends_with('\\') {
        s.pop();
    }
    // R7：全串小写。
    s.to_lowercase()
}

/// 从 Run 值条目中提取（路径段, 参数段）。
///
/// 成对引号形态：引号内为路径，其后内容视为用户自加参数（AF-7/BF-6：
/// 重写时保留，尊重用户对自启项的手工定制）；残缺引号与无引号形态下
/// 首个空白后的内容不可信（既可能是参数也可能是被截断的路径，
/// CreateProcess 渐进解析歧义，BF-8），一律不保留参数、判不匹配后收敛
/// 重写为带引号规范式。
pub(crate) fn extract_entry_path(entry: &str) -> (&str, &str) {
    let e = entry.trim();
    if let Some(rest) = e.strip_prefix('"') {
        if let Some(end) = rest.find('"') {
            return (&rest[..end], rest[end + 1..].trim());
        }
        // 残缺引号：整段视为路径（规范化后与规范式不等 → 收敛重写）。
        return (e, "");
    }
    // 无引号：取到首个空白（含空格路径在此截断 → 不匹配 → 收敛重写）。
    match e.find(char::is_whitespace) {
        Some(sp) => (&e[..sp], ""),
        None => (e, ""),
    }
}

/// 路径段与当前 exe 的规范化比较（双侧同走 [`normalize_path`]，
/// 比较侧即 BF-1 修复的一半）。
pub(crate) fn path_matches(stored_path: &str, exe: &std::path::Path) -> bool {
    normalize_path(stored_path) == normalize_path(&exe.to_string_lossy())
}

/// 构造 Run 值的规范写入形态（写入侧 canonical 化，BF-1）：
/// `"<标准 Win32 形态路径>"`＋可选参数段（AF-7：重写时保留用户参数）。
pub(crate) fn build_run_value(exe: &std::path::Path, args: &str) -> String {
    let mut value = format!("\"{}\"", strip_verbose_prefixes(&exe.to_string_lossy()));
    let args = args.trim();
    if !args.is_empty() {
        value.push(' ');
        value.push_str(args);
    }
    value
}

/// 启动对账决策（纯函数，C-2b/C-2c 四格决策表）。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReconcileAction {
    /// 稳态：无需动作。
    NoAction,
    /// 配置开＋值缺失 → 重建（C-2b：值缺失与路径失效同等自愈）。
    Write,
    /// 配置开＋值存在但路径不匹配 → 重写（保留成对引号形态下的参数段）。
    Rewrite { args: String },
    /// 配置关＋值存在 → 清理残留（C-2c：反向清理）。
    Remove,
}

pub(crate) fn decide(cfg_on: bool, stored: Option<&str>, exe: &std::path::Path) -> ReconcileAction {
    match (cfg_on, stored) {
        (false, Some(_)) => ReconcileAction::Remove,
        (false, None) => ReconcileAction::NoAction,
        (true, None) => ReconcileAction::Write,
        (true, Some(entry)) => {
            let (path, args) = extract_entry_path(entry);
            if path_matches(path, exe) {
                ReconcileAction::NoAction
            } else {
                ReconcileAction::Rewrite {
                    args: args.to_string(),
                }
            }
        }
    }
}

/// 启动对账结果（调用方据此落日志/告警，C-2e：成功静默＋bootlog 双路径，
/// 失败必须可见——复用既有 Alert 通道，零新增通知路径）。
#[derive(Debug)]
pub enum ReconcileOutcome {
    /// 稳态：无需动作（占启动绝大多数，零感知）。
    NoAction,
    /// debug 构建跳过自动注册表写（C-2f 开发态保护）。
    SkippedDev,
    /// 注册表读取失败——不写、不回退配置；调用方 bootlog＋Alert 一次。
    ReadFailed(String),
    /// 成功修复——old→new 双路径（bootlog 一行，诊断可达）。
    Repaired {
        old: Option<String>,
        new: Option<String>,
    },
    /// 写/删失败——`registry_on` 为失败后注册表现状（值存在即 true），
    /// 调用方按既有对账回路失败语义回退共享配置＋persist＋Alert（BF-5）。
    WriteFailed { registry_on: bool, reason: String },
}

/// 启动一次性对账（U-7 核心，C-1）。
///
/// 在 GuiState 初始化**之前**执行一次：注册表收敛到「配置意图 × 当前
/// exe」，四格状态全收口（路径陈旧→重写；配置开＋值缺失→重建；配置关
/// ＋值残留→清理；稳态→不动）。既有 cfg_changed 对账回路为事件驱动
/// （v0.3.15 起 500ms 常驻心跳退役），无设备会话可能整会话不触发，不能
/// 作为唯一收敛时机（R5 红蓝对抗 BF-3 实测）——本函数是唯一时机有界
/// 的收口点；进程级一次、无定时器、无跨运行状态（幂等，C-4 无需迁移
/// 标记）。debug 构建跳过一切自动注册表写（C-2f）。
pub fn reconcile(cfg_on: bool, exe: &std::path::Path) -> ReconcileOutcome {
    if cfg!(debug_assertions) {
        // C-2f：debug 构建不做任何自动注册表写（防 target\debug 调试
        // 二进制污染真实自启项）；读取与显示不受影响。
        return ReconcileOutcome::SkippedDev;
    }
    let stored = match imp::read_entry() {
        Ok(v) => v,
        Err(e) => return ReconcileOutcome::ReadFailed(e),
    };
    match decide(cfg_on, stored.as_deref(), exe) {
        ReconcileAction::NoAction => ReconcileOutcome::NoAction,
        ReconcileAction::Write => match imp::enable_with_args(exe, "") {
            Ok(()) => ReconcileOutcome::Repaired {
                old: None,
                new: Some(build_run_value(exe, "")),
            },
            Err(reason) => ReconcileOutcome::WriteFailed {
                // 写前值缺失，写失败 → 仍缺失。
                registry_on: false,
                reason,
            },
        },
        ReconcileAction::Rewrite { args } => {
            let new = build_run_value(exe, &args);
            match imp::enable_with_args(exe, &args) {
                Ok(()) => ReconcileOutcome::Repaired {
                    old: stored,
                    new: Some(new),
                },
                Err(reason) => ReconcileOutcome::WriteFailed {
                    // 写前旧值存在，写失败 → 旧值仍在。
                    registry_on: true,
                    reason,
                },
            }
        }
        ReconcileAction::Remove => match imp::disable() {
            Ok(()) => ReconcileOutcome::Repaired {
                old: stored,
                new: None,
            },
            Err(reason) => ReconcileOutcome::WriteFailed {
                // 删失败 → 残留值仍在。
                registry_on: true,
                reason,
            },
        },
    }
}

#[cfg(windows)]
mod imp {
    use super::RUN_VALUE_NAME;
    use std::path::Path;
    use windows::core::PCWSTR;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
        HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ, REG_VALUE_TYPE,
    };

    const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn open_run_key(
        access: windows::Win32::System::Registry::REG_SAM_FLAGS,
    ) -> Result<HKEY, String> {
        let sub = wide(RUN_KEY);
        let mut hkey = HKEY::default();
        unsafe {
            let r = RegOpenKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR::from_raw(sub.as_ptr()),
                None,
                access,
                &mut hkey,
            );
            if r.is_err() {
                return Err(format!("RegOpenKeyExW failed: {r:?}"));
            }
        }
        Ok(hkey)
    }

    /// 读取 Run 值完整内容（U-7）。三态：`Ok(None)`＝值不存在（或零长度，
    /// 维持原 is_enabled「false 即未开启」口径）；`Ok(Some(s))`＝值内容；
    /// `Err(e)`＝读取失败。
    ///
    /// 实现说明（C-3 裁决，R5-AF-1 合规路线落地）：读值并入 is_enabled 的
    /// 既有 unsafe 块——第一段 RegQueryValueExW 即原 is_enabled 的 size
    /// 查询（原样保留），第二段带缓冲区读内容；本文件 unsafe 块数较基线
    /// **零新增**。
    pub fn read_entry() -> Result<Option<String>, String> {
        let hkey = open_run_key(KEY_QUERY_VALUE)?;
        let name = wide(RUN_VALUE_NAME);
        let mut ty = REG_VALUE_TYPE(0);
        let mut size = 0u32;
        unsafe {
            let r = RegQueryValueExW(
                hkey,
                PCWSTR::from_raw(name.as_ptr()),
                None,
                Some(&mut ty),
                None,
                Some(&mut size),
            );
            if r.0 == 2 {
                // ERROR_FILE_NOT_FOUND：值不存在（与 disable 的幂等口径一致）。
                let _ = RegCloseKey(hkey);
                return Ok(None);
            }
            if r.is_err() {
                let _ = RegCloseKey(hkey);
                return Err(format!("RegQueryValueExW failed: {r:?}"));
            }
            if size == 0 {
                // 零长度值：维持原 is_enabled 语义（false 即「未开启」）。
                let _ = RegCloseKey(hkey);
                return Ok(None);
            }
            let mut buf = vec![0u16; (size as usize).div_ceil(2).max(1)];
            let mut size2 = (buf.len() * 2) as u32;
            let r2 = RegQueryValueExW(
                hkey,
                PCWSTR::from_raw(name.as_ptr()),
                None,
                Some(&mut ty),
                Some(buf.as_mut_ptr() as *mut u8),
                Some(&mut size2),
            );
            let _ = RegCloseKey(hkey);
            if r2.is_err() {
                return Err(format!("RegQueryValueExW failed: {r2:?}"));
            }
            // REG_SZ 理论上 NUL 结尾，但注册表不保证（MSDN）——截首个 NUL。
            Ok(Some(
                String::from_utf16_lossy(&buf)
                    .split('\0')
                    .next()
                    .unwrap_or("")
                    .to_string(),
            ))
        }
    }

    pub fn is_enabled() -> bool {
        // U-7：改为 read_entry 的安全包装——「值存在即开」语义不变
        //（C-2d：显示语义不动；零长度值原样判关，与基线一致）。
        read_entry().is_ok_and(|v| v.is_some())
    }

    pub fn enable(exe_path: &Path) -> Result<(), String> {
        enable_with_args(exe_path, "")
    }

    /// U-7：写入 Run 值（写入侧 canonical 化，BF-1——剥 `\\?\` /
    /// `\\?\UNC\` 前缀，恒为标准 Win32 形态）；重写时保留用户自加
    /// 参数段（AF-7/BF-6）。
    pub fn enable_with_args(exe_path: &Path, args: &str) -> Result<(), String> {
        let hkey = open_run_key(KEY_SET_VALUE)?;
        let value = wide(&super::build_run_value(exe_path, args));
        let bytes: &[u8] =
            unsafe { std::slice::from_raw_parts(value.as_ptr() as *const u8, value.len() * 2) };
        let name = wide(RUN_VALUE_NAME);
        unsafe {
            let r = RegSetValueExW(
                hkey,
                PCWSTR::from_raw(name.as_ptr()),
                None,
                REG_SZ,
                Some(bytes),
            );
            let _ = RegCloseKey(hkey);
            if r.is_err() {
                return Err(format!("RegSetValueExW failed: {r:?}"));
            }
        }
        Ok(())
    }

    pub fn disable() -> Result<(), String> {
        let hkey = open_run_key(KEY_SET_VALUE)?;
        let name = wide(RUN_VALUE_NAME);
        unsafe {
            let r = RegDeleteValueW(hkey, PCWSTR::from_raw(name.as_ptr()));
            let _ = RegCloseKey(hkey);
            // 值不存在不算失败（幂等）。
            if r.is_err() && r.0 != 2 {
                return Err(format!("RegDeleteValueW failed: {r:?}"));
            }
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod imp {
    use std::path::Path;
    const UNSUPPORTED: &str = "autostart is only supported on Windows";
    pub fn is_enabled() -> bool {
        false
    }
    pub fn read_entry() -> Result<Option<String>, String> {
        Err(UNSUPPORTED.into())
    }
    pub fn enable(_exe_path: &Path) -> Result<(), String> {
        Err(UNSUPPORTED.into())
    }
    pub fn enable_with_args(_exe_path: &Path, _args: &str) -> Result<(), String> {
        Err(UNSUPPORTED.into())
    }
    pub fn disable() -> Result<(), String> {
        Err(UNSUPPORTED.into())
    }
}

pub use imp::{disable, enable, enable_with_args, is_enabled, read_entry};

/// 将注册表自启态对齐到 `on`（P2-3：reset-all 等批量重置路径，
/// 使 HKCU Run 键与默认值一致，不与 TOML 分叉）。
pub fn sync(on: bool, exe_path: &std::path::Path) -> Result<(), String> {
    if on {
        enable(exe_path)
    } else {
        disable()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exe(p: &str) -> &std::path::Path {
        std::path::Path::new(p)
    }

    fn entry_matches(entry: &str, exe_path: &std::path::Path) -> bool {
        let (path, _) = extract_entry_path(entry);
        path_matches(path, exe_path)
    }

    // ===== R1~R7 规则集（C-2a）=====

    #[test]
    fn f01_quoted_entry_matches() {
        assert!(entry_matches(
            r#""C:\Apps\volttray-tray.exe""#,
            exe(r"C:\Apps\volttray-tray.exe")
        ));
    }

    #[test]
    fn f02_unclosed_quote_no_match() {
        // 残缺引号保守保留 → 判不等 → 收敛重写（R2 划界锁定）。
        assert!(!entry_matches(
            r#""C:\Apps\volttray-tray.exe"#,
            exe(r"C:\Apps\volttray-tray.exe")
        ));
    }

    #[test]
    fn f03_case_insensitive() {
        assert!(entry_matches(
            r#""C:\APPS\VoltTray-TRAY.exe""#,
            exe(r"c:\apps\volttray-tray.exe")
        ));
    }

    #[test]
    fn f04_verbose_prefix_stored_matches() {
        assert!(entry_matches(
            r#""\\?\C:\Apps\volttray-tray.exe""#,
            exe(r"C:\Apps\volttray-tray.exe")
        ));
        // 双向：current_exe 返回 \\?\ 形态时同样判匹配（比较侧 BF-1）。
        assert!(entry_matches(
            r#""C:\Apps\volttray-tray.exe""#,
            exe(r"\\?\C:\Apps\volttray-tray.exe")
        ));
    }

    #[test]
    fn f05_unc_verbose_prefix_matches() {
        assert!(entry_matches(
            r#""\\?\UNC\server\share\volttray-tray.exe""#,
            exe(r"\\server\share\volttray-tray.exe")
        ));
    }

    #[test]
    fn f06_forward_slashes_match() {
        assert!(entry_matches(
            r#""C:/Apps/volttray-tray.exe""#,
            exe(r"C:\Apps\volttray-tray.exe")
        ));
    }

    #[test]
    fn f07_doubled_backslash_collapsed_and_unc_preserved() {
        assert!(entry_matches(
            r#""C:\\Apps\\volttray-tray.exe""#,
            exe(r"C:\Apps\volttray-tray.exe")
        ));
        // UNC 前导 \\ 不被折叠（R4）。
        assert_eq!(
            normalize_path(r"\\server\share\x.exe"),
            normalize_path(r"\\server\share\x.exe")
        );
        assert_ne!(
            normalize_path(r"\\server\share\x.exe"),
            normalize_path(r"\server\share\x.exe")
        );
    }

    #[test]
    fn f08_trailing_backslash_matches() {
        assert!(entry_matches(
            r#""C:\Apps\volttray-tray.exe\""#,
            exe(r"C:\Apps\volttray-tray.exe")
        ));
    }

    #[test]
    fn f09_whitespace_trimmed() {
        assert!(entry_matches(
            "  \"C:\\Apps\\volttray-tray.exe\"  \r\n",
            exe(r"C:\Apps\volttray-tray.exe")
        ));
    }

    #[test]
    fn f10_short_name_8_3_no_match() {
        // 8.3 短名不展开（有意划界）——判不等 → 收敛重写为长名规范式。
        assert!(!entry_matches(
            r#""C:\PROGRA~1\volttray-tray.exe""#,
            exe(r"C:\Program Files\volttray-tray.exe")
        ));
    }

    #[test]
    fn f11_quoted_entry_with_args_matches() {
        assert!(entry_matches(
            r#""C:\Apps\volttray-tray.exe" --minimized"#,
            exe(r"C:\Apps\volttray-tray.exe")
        ));
    }

    #[test]
    fn f12_unquoted_no_space_matches() {
        assert!(entry_matches(
            r"C:\Apps\volttray-tray.exe",
            exe(r"C:\Apps\volttray-tray.exe")
        ));
    }

    #[test]
    fn f13_unquoted_with_space_no_match() {
        // 无引号含空格 → 截断判不匹配 → 收敛重写为带引号规范式（BF-8）。
        assert!(!entry_matches(
            r"C:\Program Files\volttray-tray.exe",
            exe(r"C:\Program Files\volttray-tray.exe")
        ));
    }

    #[test]
    fn f14_different_path_no_match() {
        assert!(!entry_matches(
            r#""C:\Old\volttray-tray-v0.3.18.exe""#,
            exe(r"C:\New\volttray-tray-v0.3.19.exe")
        ));
    }

    #[test]
    fn f15_equal_length_version_rename_no_match() {
        // 用户真实场景钉死（B 稿 T4）：等长版本号换名必须判不匹配，
        // 防实现退化成长度/哈希比较。
        assert!(!entry_matches(
            r#""C:\Apps\volttray-tray-v0.3.18.exe""#,
            exe(r"C:\Apps\volttray-tray-v0.3.19.exe")
        ));
    }

    #[test]
    fn f16_empty_entry_no_match() {
        assert!(!entry_matches("", exe(r"C:\Apps\volttray-tray.exe")));
        assert!(!entry_matches("   ", exe(r"C:\Apps\volttray-tray.exe")));
    }

    // ===== extract_entry_path / strip_verbose_prefixes / build_run_value =====

    #[test]
    fn f17_extract_entry_path_forms() {
        let (p, a) = extract_entry_path(r#""C:\a\b.exe" --minimized --flag=1"#);
        assert_eq!(p, r"C:\a\b.exe");
        assert_eq!(a, "--minimized --flag=1");
        // 无引号截断：空白后内容不视为参数（不可信，BF-8）。
        let (p2, a2) = extract_entry_path(r"C:\Program Files\b.exe");
        assert_eq!(p2, r"C:\Program");
        assert_eq!(a2, "");
        // 残缺引号：整段为路径、无参数。
        let (p3, a3) = extract_entry_path(r#""C:\a\b.exe"#);
        assert_eq!(p3, r#""C:\a\b.exe"#);
        assert_eq!(a3, "");
        // 无引号无参数。
        let (p4, a4) = extract_entry_path(r"C:\a\b.exe");
        assert_eq!(p4, r"C:\a\b.exe");
        assert_eq!(a4, "");
    }

    #[test]
    fn f18_strip_verbose_prefixes_variants() {
        assert_eq!(strip_verbose_prefixes(r"\\?\C:\x\y.exe"), r"C:\x\y.exe");
        assert_eq!(
            strip_verbose_prefixes(r"\\?\UNC\srv\share\y.exe"),
            r"\\srv\share\y.exe"
        );
        // 大小写不敏感。
        assert_eq!(
            strip_verbose_prefixes(r"\\?\unc\srv\share\y.exe"),
            r"\\srv\share\y.exe"
        );
        assert_eq!(
            strip_verbose_prefixes(r"\\?\C:\x\y.exe".to_uppercase().as_str()),
            r"C:\X\Y.EXE"
        );
        // 无前缀原样返回（保留原大小写）。
        assert_eq!(strip_verbose_prefixes(r"C:\Apps\Y.exe"), r"C:\Apps\Y.exe");
    }

    #[test]
    fn f19_build_run_value_canonical_and_args() {
        // 写入侧 canonical 化（BF-1）：\\?\ 形态写入前剥前缀。
        assert_eq!(
            build_run_value(exe(r"\\?\C:\Apps\volttray-tray.exe"), ""),
            r#""C:\Apps\volttray-tray.exe""#
        );
        assert_eq!(
            build_run_value(exe(r"\\?\UNC\srv\share\volttray-tray.exe"), ""),
            r#""\\srv\share\volttray-tray.exe""#
        );
        // 参数保留（AF-7/BF-6）。
        assert_eq!(
            build_run_value(exe(r"C:\Apps\volttray-tray.exe"), "--minimized"),
            r#""C:\Apps\volttray-tray.exe" --minimized"#
        );
        // 空参数无多余空白。
        assert_eq!(
            build_run_value(exe(r"C:\Apps\volttray-tray.exe"), "   "),
            r#""C:\Apps\volttray-tray.exe""#
        );
    }

    #[test]
    fn f23_root_form_preserved() {
        // R6：C:\ 根形式不剥（防御性，exe 路径实际不会命中）。
        assert_eq!(normalize_path(r"C:\"), "c:\\");
    }

    #[test]
    fn f24_normalize_empty_and_plain() {
        assert_eq!(normalize_path(""), "");
        assert_eq!(normalize_path(r"C:\Apps\X.exe"), r"c:\apps\x.exe");
    }

    // ===== decide 四格决策表（C-2b/C-2c）=====

    #[test]
    fn f20_decide_table() {
        let e = exe(r"C:\Apps\volttray-tray.exe");
        assert_eq!(decide(false, None, e), ReconcileAction::NoAction);
        assert_eq!(
            decide(false, Some(r#""C:\Old\x.exe""#), e),
            ReconcileAction::Remove
        );
        assert_eq!(decide(true, None, e), ReconcileAction::Write);
        assert_eq!(
            decide(true, Some(r#""C:\Apps\volttray-tray.exe""#), e),
            ReconcileAction::NoAction
        );
        assert_eq!(
            decide(true, Some(r#""C:\Old\x.exe""#), e),
            ReconcileAction::Rewrite {
                args: String::new()
            }
        );
    }

    #[test]
    fn f21_decide_rewrite_preserves_args() {
        let e = exe(r"C:\Apps\volttray-tray.exe");
        assert_eq!(
            decide(true, Some(r#""C:\Old\x.exe" --minimized"#), e),
            ReconcileAction::Rewrite {
                args: "--minimized".to_string()
            }
        );
        // 无引号含空格的旧值：截断不匹配，参数段不可信不保留（BF-8）。
        assert_eq!(
            decide(true, Some(r"C:\Old\dir with space\x.exe"), e),
            ReconcileAction::Rewrite {
                args: String::new()
            }
        );
    }

    #[test]
    fn f22_rewritten_value_is_idempotent_fixpoint() {
        // 幂等闭环（A 稿 T23 思想）：重写产物再喂回 decide → NoAction，
        // 锁定「不会每启动重复重写」。
        let e = exe(r"C:\Apps\volttray-tray.exe");
        let rewritten = build_run_value(e, "--minimized");
        assert_eq!(decide(true, Some(&rewritten), e), ReconcileAction::NoAction);
        let rewritten_plain = build_run_value(e, "");
        assert_eq!(
            decide(true, Some(&rewritten_plain), e),
            ReconcileAction::NoAction
        );
    }
}
