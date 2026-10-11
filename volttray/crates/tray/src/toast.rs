//! WinRT toast 通知（P0-5：AUMID 通道）。
//!
//! 失败时上层降级为 tooltip 提示 + 诊断事件（P0-5 裁决的降级路径）。

use volttray_app::aumid::AUMID;
use windows::core::HSTRING;
use windows::Data::Xml::Dom::XmlDocument;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

pub fn notify(title: &str, body: &str, silent: bool) -> Result<(), String> {
    let audio = if silent {
        "<audio silent=\"true\"/>"
    } else {
        ""
    };
    let xml = format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual>{}</toast>",
        xml_escape(title),
        xml_escape(body),
        audio
    );
    let doc = XmlDocument::new().map_err(|e| format!("XmlDocument::new failed: {e}"))?;
    doc.LoadXml(&HSTRING::from(xml.as_str()))
        .map_err(|e| format!("LoadXml failed: {e}"))?;
    let toast = ToastNotification::CreateToastNotification(&doc)
        .map_err(|e| format!("CreateToastNotification failed: {e}"))?;
    let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))
        .map_err(|e| format!("CreateToastNotifierWithId failed: {e}"))?;
    notifier
        .Show(&toast)
        .map_err(|e| format!("toast Show failed: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_special_chars() {
        assert_eq!(xml_escape("a<b>&\"c\""), "a&lt;b&gt;&amp;&quot;c&quot;");
    }
}
