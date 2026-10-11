//! 枚举本机 Logitech HID++ 候选接口（诊断辅助）。
//! 运行：cargo run -p volttray-transport --example enum_hid --features hid

fn main() {
    let api = match hidapi::HidApi::new() {
        Ok(api) => api,
        Err(e) => {
            eprintln!("hidapi init failed: {e}");
            std::process::exit(1);
        }
    };
    let devices = volttray_transport::hid_impl::enumerate_logitech(&api);
    if devices.is_empty() {
        println!("no Logitech HID++ interface found (VID 0x046D, usage page 0xFF43/0xFF00)");
        return;
    }
    for d in &devices {
        println!(
            "vid=0x{:04X} pid=0x{:04X} usage_page=0x{:04X} iface={} path={} product={}",
            d.vendor_id(),
            d.product_id(),
            d.usage_page(),
            d.interface_number(),
            d.path().to_string_lossy(),
            d.product_string().unwrap_or("?"),
        );
    }
    println!("total: {}", devices.len());
}
