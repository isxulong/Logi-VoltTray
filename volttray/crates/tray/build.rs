//! 嵌入 asInvoker + PerMonitorV2 manifest（PRD 非功能需求）。
//! v0.3.3 #1：EXE 内部版本信息统一命名 LogiVoltTray（无空格）。
//! v0.3.5 #2（D-53）：嵌入应用图标（assets/volttray.ico，多尺寸
//! 16/24/32/48/64/128/256）与完整版本资源（FileVersion/ProductVersion
//! 字符串 + FILEVERSION/PRODUCTVERSION 数值）——任务栏 / Alt-Tab /
//! 文件属性均显示图标与版本。

fn main() {
    println!("cargo:rerun-if-changed=volttray.exe.manifest");
    println!("cargo:rerun-if-changed=../../assets/volttray.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        // v0.3.8：版本字符串从 CARGO_PKG_VERSION 派生（此前手工同步，
        // 容易与 workspace 版本脱节）；数值版本按 major.minor.patch 打包。
        let pkg_ver =
            std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".to_string());
        let nums: Vec<u64> = pkg_ver
            .split('.')
            .map(|v| v.parse().unwrap_or(0))
            .collect();
        let (major, minor, patch) = match nums.as_slice() {
            [ma, mi, pa] => (*ma, *mi, *pa),
            _ => (0, 0, 0),
        };
        let ver_str = format!("{pkg_ver}.0");
        let ver_num = (major << 48) | (minor << 32) | (patch << 16);
        let mut res = winres::WindowsResource::new();
        res.set_manifest_file("volttray.exe.manifest");
        // 图标：winres 以资源名 "1" 写入，运行时 LoadIconW(MAKEINTRESOURCEW(1))
        // 取之（settings 窗口 WM_CREATE / WNDCLASS 共用）。
        res.set_icon("../../assets/volttray.ico");
        res.set("ProductName", "LogiVoltTray");
        res.set("FileDescription", "LogiVoltTray 托盘电量显示器");
        // v0.3.4 遗留 + v0.3.5 #2：补齐版本字段（字符串 + 数值双写，
        // 文件属性页与资源管理器列都读得到）。
        res.set("FileVersion", &ver_str);
        res.set("ProductVersion", &ver_str);
        res.set_version_info(winres::VersionInfo::FILEVERSION, ver_num);
        res.set_version_info(winres::VersionInfo::PRODUCTVERSION, ver_num);
        if let Err(e) = res.compile() {
            // 清单嵌入失败不阻断构建（运行时 DPI 设置为兜底），但留警告。
            println!("cargo:warning=winres manifest embed failed: {e}");
        }
    }
}
