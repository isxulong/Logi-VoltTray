//! E22（报告口径）：wake 裁决行稳态采样器——纯逻辑（无 Win32 依赖），
//! 跨平台编译以便在非 Windows 环境跑单元测试（同 blink / frame_lru 先例）。
//!
//! 口径（docs/experience/01-engineering-lessons.md E33，v0.3.17 R-4）：
//! 稳态日志采样带计数、跃迁日志全量。BF-2 只覆盖 hit ∧ 未装填 ∧
//! stay-asleep；E10/E11 拆桶后 miss-* 各桶的「miss ∧ 未装填 ∧
//! stay-asleep」同样是鼠标活动稳态下的高频零信息行（BT/有线系统性
//! 失配场景每次鼠标活动一条），纳入同一稳态采样：同桶 60s 窗口只落
//! 第 1 条、压制计数合并进下一条；桶之间切换视为跃迁——本行全量
//! 落盘，并把上一桶被压制的计数随跃迁冲出（不丢数）。装填 /
//! probe-round / core-lock-failed 等非稳态行仍逐条全量（由调用方
//! 判定并先调 leave_steady 冲出 pending 计数）。

use std::time::{Duration, Instant};

/// 稳态窗口：与 MissLogSampler 同为 60s（每分钟 1 条 + 计数）。
const STEADY_WINDOW: Duration = Duration::from_secs(60);

/// wake 裁决行稳态采样：稳态采样带计数、跃迁全量。
pub struct SteadyDecisionSampler {
    /// 当前稳态桶（attribution 字符串，如 "hit" / "miss-zero-match"）。
    bucket: Option<String>,
    window_start: Option<Instant>,
    suppressed: u64,
}

impl SteadyDecisionSampler {
    pub fn new() -> Self {
        Self {
            bucket: None,
            window_start: None,
            suppressed: 0,
        }
    }

    /// 冲出当前桶的 pending 压制计数（不丢数）；无则 None。
    fn flush_suppressed(&mut self) -> Option<String> {
        if self.suppressed == 0 {
            return None;
        }
        let n = self.suppressed;
        self.suppressed = 0;
        let bucket = self.bucket.as_deref().unwrap_or("unknown");
        Some(format!(
            "wake decision steady-state: bucket={bucket} suppressed {n} similar in last 60s"
        ))
    }

    /// 记录一条稳态裁决行，返回应落盘的行（0~2 条：同桶窗口内压制
    /// 返回空；桶跃迁时先冲出上一桶压制计数，再全量落本行）。
    pub fn note_steady(&mut self, bucket: &str, msg: &str, now: Instant) -> Vec<String> {
        let mut out = Vec::new();
        if self.bucket.as_deref() != Some(bucket) {
            // 桶跃迁：冲出上一桶 pending 计数（不丢数），本行全量落。
            if let Some(line) = self.flush_suppressed() {
                out.push(line);
            }
            self.bucket = Some(bucket.to_string());
            self.window_start = Some(now);
            out.push(msg.to_string());
            return out;
        }
        match self.window_start {
            Some(t) if now.duration_since(t) < STEADY_WINDOW => {
                self.suppressed = self.suppressed.saturating_add(1);
            }
            _ => {
                // 同桶窗口届满：压制计数合并进本条（沿用 MissLogSampler
                // 的「(suppressed N similar in last 60s)」后缀口径）。
                let n = self.suppressed;
                self.suppressed = 0;
                self.window_start = Some(now);
                if n > 0 {
                    out.push(format!("{msg} (suppressed {n} similar in last 60s)"));
                } else {
                    out.push(msg.to_string());
                }
            }
        }
        out
    }

    /// 离开稳态（装填 / probe-round / core-lock-failed 等非稳态行全量
    /// 落盘前调用）：若稳态桶内有 pending 压制计数则冲出，随后复位，
    /// 下次进入稳态按桶跃迁处理（首行全量）。
    pub fn leave_steady(&mut self) -> Option<String> {
        self.bucket.as_ref()?;
        let line = self.flush_suppressed();
        self.bucket = None;
        self.window_start = None;
        line
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// E22：同桶稳态 60s 窗口只落第 1 条，压制计数合并进下一条。
    #[test]
    fn same_bucket_steady_sampled_with_count() {
        let mut s = SteadyDecisionSampler::new();
        let t0 = Instant::now();
        assert_eq!(
            s.note_steady("miss-zero-match", "m", t0),
            vec!["m".to_string()]
        );
        assert!(s
            .note_steady("miss-zero-match", "m", t0 + Duration::from_secs(10))
            .is_empty());
        assert!(s
            .note_steady("miss-zero-match", "m", t0 + Duration::from_secs(20))
            .is_empty());
        assert_eq!(
            s.note_steady("miss-zero-match", "m", t0 + Duration::from_secs(61)),
            vec!["m (suppressed 2 similar in last 60s)".to_string()]
        );
    }

    /// E22：桶跃迁全量——上一桶 pending 计数冲出、本行不采样。
    #[test]
    fn bucket_transition_logs_full_and_flushes_count() {
        let mut s = SteadyDecisionSampler::new();
        let t0 = Instant::now();
        s.note_steady("miss-zero-match", "a", t0);
        s.note_steady("miss-zero-match", "a", t0 + Duration::from_secs(5));
        s.note_steady("miss-zero-match", "a", t0 + Duration::from_secs(6));
        let out = s.note_steady("miss-no-path", "b", t0 + Duration::from_secs(7));
        assert_eq!(
            out,
            vec![
                "wake decision steady-state: bucket=miss-zero-match suppressed 2 similar in last 60s"
                    .to_string(),
                "b".to_string()
            ]
        );
        // 新桶进入自己的稳态窗口。
        assert!(s
            .note_steady("miss-no-path", "b", t0 + Duration::from_secs(8))
            .is_empty());
    }

    /// E22：离开稳态冲出 pending 计数并复位；再次进入稳态首行全量。
    #[test]
    fn leave_steady_flushes_and_resets() {
        let mut s = SteadyDecisionSampler::new();
        let t0 = Instant::now();
        s.note_steady("hit", "h", t0);
        s.note_steady("hit", "h", t0 + Duration::from_secs(3));
        assert_eq!(
            s.leave_steady().as_deref(),
            Some("wake decision steady-state: bucket=hit suppressed 1 similar in last 60s")
        );
        // 复位后 leave_steady 幂等为空。
        assert_eq!(s.leave_steady(), None);
        // 再进稳态（同桶）按跃迁处理：首行全量、重新开窗。
        assert_eq!(
            s.note_steady("hit", "h", t0 + Duration::from_secs(4)),
            vec!["h".to_string()]
        );
    }

    /// E22：无 pending 计数时跃迁 / 离开不产生多余行。
    #[test]
    fn no_pending_count_no_extra_line() {
        let mut s = SteadyDecisionSampler::new();
        let t0 = Instant::now();
        s.note_steady("hit", "h", t0);
        assert_eq!(
            s.note_steady("miss-ambiguous", "a", t0 + Duration::from_secs(1)),
            vec!["a".to_string()]
        );
        assert_eq!(s.leave_steady(), None);
    }
}
