//! E8（红蓝对抗终审）：自 main.rs 拆出的持久化叶子模块。
//!
//! 仅结构搬动（模块归属与可见性调整），函数体逐字节不变——行为基线以
//! 合成 HID 序列 + 抽取函数逐字节比对验证。包含：平面 TOML 标量键就地
//! 改写（配置所有权在 app，托盘/设置窗口只触发持久化）、bool 便捷封装、
//! tmp→rename 原子落盘（E5：tmp 名带 pid＋进程内计数器唯一后缀）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::bootlog;

/// 平面 TOML 的标量键就地改写；键不存在则追加到文件尾（配置所有权在 app，
/// 托盘/设置窗口只触发持久化）。读取失败（如文件不存在）则只写该键。
pub(crate) fn persist_kv(path: &Path, key: &str, value: &str) {
    // v0.3.17（一轮 P0-⑤）：落盘改 tmp→rename 原子写（写中途崩溃
    // 不留半截 TOML）；键匹配由前缀改等号锚定（避免「thresholds」
    // 误中同前缀键）；写失败落 bootlog，不再静默吞错。
    if let Ok(text) = std::fs::read_to_string(path) {
        let mut out = String::with_capacity(text.len() + 16);
        let mut done = false;
        for line in text.lines() {
            let line_key = line.trim_start().split('=').next().map(str::trim);
            if !done && line_key == Some(key) {
                out.push_str(&format!("{key} = {value}"));
                done = true;
            } else {
                out.push_str(line);
            }
            out.push('\n');
        }
        if !done {
            out.push_str(&format!("{key} = {value}\n"));
        }
        if let Err(e) = write_tmp_rename(path, &out) {
            bootlog::note(&format!("persist {key} to {} failed: {e}", path.display()));
        }
    } else if let Err(e) = write_tmp_rename(path, &format!("{key} = {value}\n")) {
        bootlog::note(&format!("persist {key} to {} failed: {e}", path.display()));
    }
}

pub(crate) fn persist_bool(path: &Path, key: &str, value: bool) {
    persist_kv(path, key, if value { "true" } else { "false" });
}

/// tmp→rename 落盘：写中途失败不留半截目标文件；失败清 tmp。
/// rename 在 Windows 上原子覆盖既有目标，失败时旧文件保留、错误
/// 向上返回。
/// E5（P1a，红蓝对抗终审）：tmp 名带 pid＋进程内计数器唯一后缀——
/// 同路径并发写（导出与配置落盘竞争等）不再共用同一 tmp 名互踩。
pub(crate) fn write_tmp_rename(path: &Path, content: &str) -> std::io::Result<()> {
    static TMP_SEQ: AtomicU32 = AtomicU32::new(0);
    let mut tmp_os = path.as_os_str().to_owned();
    tmp_os.push(format!(
        ".tmp-{}-{}",
        std::process::id(),
        TMP_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let tmp = PathBuf::from(tmp_os);
    let result = (|| {
        std::fs::write(&tmp, content)?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "volttray-diag-test-{}-{tag}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        p
    }

    /// T-01(a) 故障注入：rename 失败时旧目标逐字节保留、返回 Err、
    /// tmp 被清理。注入形态＝目标被目录占位（Linux EISDIR / Windows
    /// Access Denied，跨平台必失败；Windows 句柄占用形态不可移植构造，
    /// 由 CI windows 环境承载）。
    #[test]
    fn write_tmp_rename_failure_keeps_old_target_and_cleans_tmp() {
        let root = temp_root("t01a");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("target.json");
        std::fs::create_dir(&path).unwrap();
        let inner = path.join("keep.txt");
        std::fs::write(&inner, b"old-bytes").unwrap();
        let result = write_tmp_rename(&path, "new-content");
        assert!(result.is_err(), "rename 到目录占位目标必须返回 Err");
        assert!(inner.is_file(), "旧目标（目录形态及其内容）必须保留");
        assert_eq!(
            std::fs::read(&inner).unwrap(),
            b"old-bytes",
            "旧目标逐字节保留"
        );
        // E5：tmp 名带 pid＋计数器唯一后缀，按前缀检查残留。
        assert!(
            !std::fs::read_dir(&root)
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|e| e
                    .file_name()
                    .to_string_lossy()
                    .starts_with("target.json.tmp")),
            "失败后 tmp 必须被清理"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
    /// T-01(b) 原子性：目标已存在时覆盖写，任何可观察时刻目标存在且
    /// 内容完整——成功＝新值；成功后无 tmp 残留（失败＝旧值由 (a) 承载）。
    #[test]
    fn write_tmp_rename_overwrite_is_atomic() {
        let root = temp_root("t01b");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("target.json");
        std::fs::write(&path, "old-content").unwrap();
        write_tmp_rename(&path, "new-content").expect("覆盖写必须成功");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "new-content",
            "成功后目标＝新值"
        );
        // E5：tmp 名带 pid＋计数器唯一后缀，按前缀检查残留。
        assert!(
            !std::fs::read_dir(&root)
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|e| e
                    .file_name()
                    .to_string_lossy()
                    .starts_with("target.json.tmp")),
            "成功后无 tmp 残留"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
    /// E5（P1a）：双线程同路径并发写——tmp 名带 pid＋计数器唯一
    /// 后缀，两写者不共用 tmp 名互踩；每次 rename 原子覆盖，任何
    /// 可观察时刻目标都是某一写者的完整内容（非半截/混杂），全程
    /// 无 tmp 残留。
    #[test]
    fn write_tmp_rename_concurrent_same_path() {
        let root = temp_root("t01c");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("target.csv");
        let mut handles = Vec::new();
        for writer in 0..2u32 {
            let p = path.clone();
            handles.push(std::thread::spawn(move || {
                for round in 0..50u32 {
                    write_tmp_rename(&p, &format!("writer-{writer}-round-{round}"))
                        .expect("concurrent write must succeed");
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        // 终态：目标存在，内容为某一写者某一轮的完整值。
        let content = std::fs::read_to_string(&path).unwrap();
        let complete =
            (0..2u32).any(|w| (0..50u32).any(|r| content == format!("writer-{w}-round-{r}")));
        assert!(complete, "目标必须是某一完整写入值，实际: {content:?}");
        // 无 tmp 残留（按前缀检查）。
        assert!(
            !std::fs::read_dir(&root)
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|e| e
                    .file_name()
                    .to_string_lossy()
                    .starts_with("target.csv.tmp")),
            "并发写后无 tmp 残留"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
