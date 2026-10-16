//! bootlog 运行期轮转门控（v0.3.15，P2-2）。
//!
//! v0.3.14 的 1MB 轮转只在启动时检查一次——长会话（托盘常驻数周）里
//! 日志可以无限增长。本模块提供「每 N 行检查一次、超 1MB 截断重开」的
//! 纯逻辑门控 + 截断重开原语，无 Win32 依赖，跨平台可测。
//!
//! 检查粒度论证：bootlog 每行平均 ~120B（时间戳 + 结构化字段），每 64
//! 行检查一次 ≈ 每 ~7.7KB 一次 `BufWriter` 长度读取（纯内存操作，零
//! syscall），超限时日志最多超出 1MB + 64 行 ≈ 1.008MB，对诊断用途
//! 完全可接受；比「每行都 stat 文件」省三个数量级的开销。

use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};

/// 日志体积上限（与既有启动期轮转口径一致：1MB）。
pub const ROTATE_BYTES: u64 = 1_000_000;

/// 每多少行检查一次体积（论证见模块文档）。
pub const ROTATE_CHECK_LINES: u32 = 64;

pub struct RotateGate {
    limit: u64,
    check_every: u32,
    lines_since_check: u32,
    bytes_since_rotate: u64,
}

impl RotateGate {
    /// 自定义门控参数构造（生产走 default_gate；本构造器仅测试用）。
    #[cfg(test)]
    pub fn new(limit: u64, check_every: u32) -> Self {
        Self {
            limit,
            check_every: check_every.max(1),
            lines_since_check: 0,
            bytes_since_rotate: 0,
        }
    }

    /// 以「打开时已有的文件长度」初始化（启动期未轮转掉的旧内容计入）。
    pub fn default_gate(initial_len: u64) -> Self {
        Self {
            limit: ROTATE_BYTES,
            check_every: ROTATE_CHECK_LINES,
            lines_since_check: 0,
            bytes_since_rotate: initial_len,
        }
    }

    /// 登记一行已写入的字节数。返回 true = 本次该截断重开了。
    pub fn note_line(&mut self, written_bytes: u64) -> bool {
        self.bytes_since_rotate = self.bytes_since_rotate.saturating_add(written_bytes);
        self.lines_since_check += 1;
        if self.lines_since_check < self.check_every {
            return false;
        }
        self.lines_since_check = 0;
        self.bytes_since_rotate >= self.limit
    }

    /// 截断重开完成，体积计数归零。
    pub fn note_rotated(&mut self) {
        self.bytes_since_rotate = 0;
        self.lines_since_check = 0;
    }
}

/// 截断重开（原地）：先 flush 把缓冲余量落盘，再 set_len(0) + 回卷
/// 文件偏移，最后清掉 BufWriter 内部状态（seek 到 0 即同步）。
/// 顺序不可换——先 truncate 再 flush 会把旧偏移的缓冲写回文件中部。
pub fn truncate_reopen(w: &mut BufWriter<File>) -> std::io::Result<()> {
    w.flush()?;
    w.get_ref().set_len(0)?;
    w.get_mut().seek(SeekFrom::Start(0))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    /// 未达检查行数不裁决；达检查行数按体积裁决。
    #[test]
    fn gate_checks_every_n_lines() {
        let mut g = RotateGate::new(100, 4);
        // 3 行 × 30B = 90B，未到检查点。
        for _ in 0..3 {
            assert!(!g.note_line(30));
        }
        // 第 4 行 = 检查点：累计 120B ≥ 100 → 裁决轮转。
        assert!(g.note_line(30));
        // 轮转后重新计数。
        g.note_rotated();
        for _ in 0..4 {
            assert!(!g.note_line(10), "40B 远低于上限");
        }
    }

    /// 超限后不轮转则每个检查点都继续裁决（调用方失败可重试）。
    #[test]
    fn gate_repeats_verdict_until_rotated() {
        let mut g = RotateGate::new(50, 2);
        assert!(!g.note_line(60)); // 第 1 行，非检查点
        assert!(g.note_line(1)); // 检查点：61B ≥ 50
        assert!(!g.note_line(1)); // 非检查点
        assert!(g.note_line(1)); // 检查点仍超限 → 继续裁决
        g.note_rotated();
        assert!(!g.note_line(1));
        assert!(!g.note_line(1));
    }

    /// default_gate 把旧文件长度计入体积（启动期遗留 + 运行期新增）。
    #[test]
    fn default_gate_counts_initial_len() {
        let mut g = RotateGate::default_gate(ROTATE_BYTES - 10);
        g.lines_since_check = ROTATE_CHECK_LINES - 1;
        assert!(g.note_line(20), "初始长度 + 新行超限 → 裁决");
    }

    /// truncate_reopen：缓冲余量不丢、旧内容全清、偏移归零可续写。
    #[test]
    fn truncate_reopen_discards_prior_content() {
        let dir =
            std::env::temp_dir().join(format!("volttray-logrotate-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("log.txt");
        let f = File::create(&path).unwrap();
        let mut w = BufWriter::new(f);
        w.write_all(b"old content that is long enough").unwrap();
        truncate_reopen(&mut w).unwrap();
        w.write_all(b"new").unwrap();
        w.flush().unwrap();
        drop(w);
        let mut s = String::new();
        File::open(&path).unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, "new", "截断后只含新内容");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 门控驱动的运行期轮转整体链路：100B 上限 / 每 4 行检查，写 40
    /// 行 17B 行 → 至少轮转 2 次，最终文件 ≤ 4 行内容。
    #[test]
    fn gate_driven_runtime_rotation_shrinks_file() {
        let dir =
            std::env::temp_dir().join(format!("volttray-logrotate-chain-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("log.txt");
        let f = File::create(&path).unwrap();
        let mut w = BufWriter::new(f);
        let mut gate = RotateGate::new(100, 4);
        let mut rotated = 0u32;
        for i in 0..40 {
            let line = format!("line {i:02} xxxxxxxx\n"); // 17B
            w.write_all(line.as_bytes()).unwrap();
            w.flush().unwrap();
            if gate.note_line(line.len() as u64) {
                truncate_reopen(&mut w).unwrap();
                gate.note_rotated();
                rotated += 1;
            }
        }
        drop(w);
        let final_len = std::fs::metadata(&path).unwrap().len();
        assert!(
            rotated >= 2,
            "40×17B=680B 于 100B 上限应轮转 ≥2 次，实 {rotated}"
        );
        assert!(final_len <= 4 * 17, "轮转后残留 ≤4 行，实 {final_len}B");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
