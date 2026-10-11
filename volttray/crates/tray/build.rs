//! 嵌入 asInvoker + PerMonitorV2 manifest（PRD 非功能需求）。

fn main() {
    println!("cargo:rerun-if-changed=volttray.exe.manifest");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winres::WindowsResource::new();
        res.set_manifest_file("volttray.exe.manifest");
        if let Err(e) = res.compile() {
            // 清单嵌入失败不阻断构建（运行时 DPI 设置为兜底），但留警告。
            println!("cargo:warning=winres manifest embed failed: {e}");
        }
    }
}
