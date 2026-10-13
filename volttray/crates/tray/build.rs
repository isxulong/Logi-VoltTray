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
        let mut res = winres::WindowsResource::new();
        res.set_manifest_file("volttray.exe.manifest");
        // 图标：winres 以资源名 "1" 写入，运行时 LoadIconW(MAKEINTRESOURCEW(1))
        // 取之（settings 窗口 WM_CREATE / WNDCLASS 共用）。
        res.set_icon("../../assets/volttray.ico");
        res.set("ProductName", "LogiVoltTray");
        res.set("FileDescription", "LogiVoltTray 托盘电量显示器");
        // v0.3.4 遗留 + v0.3.5 #2：补齐版本字段（字符串 + 数值双写，
        // 文件属性页与资源管理器列都读得到）。
        res.set("FileVersion", "0.3.5.0");
        res.set("ProductVersion", "0.3.5.0");
        res.set_version_info(winres::VersionInfo::FILEVERSION, 0x0000_0003_0005_0000);
        res.set_version_info(winres::VersionInfo::PRODUCTVERSION, 0x0000_0003_0005_0000);
        if let Err(e) = res.compile() {
            // 清单嵌入失败不阻断构建（运行时 DPI 设置为兜底），但留警告。
            println!("cargo:warning=winres manifest embed failed: {e}");
        }
    }
}
