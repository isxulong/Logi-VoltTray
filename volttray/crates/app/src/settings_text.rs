//! E23（报告口径 A-R4②，第 1 步）：设置窗口纯逻辑文案拼写函数从
//! tray::settings 抽离到 app crate——它们本已是纯函数（无 Win32
//! 依赖），移动后跨平台可测（原测试在 cfg(windows) 的 settings.rs
//! 内，Linux/CI 主机上跑不到）。本步只移动「文本拼写」一类
//! （tier_hint_dynamic / fine_tune_gate_text / lab_hint_text 及其
//! 常量），阈值校验等其余纯逻辑留待后续窗口逐步抽离。

use crate::config::{
    Config, DEFAULT_POLL_SECS, FONT_TIER_THRESHOLD_MAX, FONT_TIER_THRESHOLD_MIN, POLL_TIERS_SECS,
};
use crate::core::{FastPollState, POLL_CHARGING};

/// 轮询间隔人性化显示（2026-10-25 用户反馈：秒不易读）。换算规则：
/// 能整除 3600 →「N 小时」，否则能整除 60 →「N 分钟」，否则保留「N 秒」。
/// 仅用于用户可见显示层——内部数值与持久化存储一律保持秒不变。
pub fn humanize_interval_secs(secs: u64) -> String {
    if secs.is_multiple_of(3600) {
        format!("{} 小时", secs / 3600)
    } else if secs.is_multiple_of(60) {
        format!("{} 分钟", secs / 60)
    } else {
        format!("{secs} 秒")
    }
}

/// FR-7（RB2）：托盘菜单「当前间隔」按配置真值显示；值不在
/// `POLL_TIERS_SECS` 档位内（如手改 config 为 900s）时追加
/// 「（非标准档）」，避免菜单显示与设置窗口下拉选中档两处文案矛盾。
/// 仅显示层一处条件分支：settings 吸附与回写逻辑不改，数值与持久化
/// 存储保持秒不变。
///
/// 例外条款登记（充电三态「立即刷新」文案方案）：实验快档
/// （experimental_per_device_polling）生效且存在符合资格的充电设备时，
/// 菜单「当前间隔」槽位经 `menu_refresh_label` 显示运行态实际节拍
/// （POLL_CHARGING，现 15 秒），配置真值以「常规 …」右注逐字保留。
/// FR-7 主规则（档外值追加「（非标准档）」）与 menu_poll_interval_label
/// 渲染路径零改动——本函数对配置值的口径不变。
pub fn menu_poll_interval_label(secs: u64) -> String {
    let base = humanize_interval_secs(secs);
    if POLL_TIERS_SECS.contains(&secs) {
        base
    } else {
        format!("{base}（非标准档）")
    }
}

/// 托盘菜单「立即刷新」项文案（充电三态；签发稿第二节六形逐字）：
/// - Off（无快档）：`立即刷新（当前间隔 {label}）`——与 FR-7 现行基线
///   逐字一致；
/// - Unified（统一快档）：`立即刷新（当前间隔 {fast}·充电中，常规 {label}）`
///   （「15 秒·充电中」中无空格，逐字）；
/// - Mixed（混合）：`立即刷新（当前间隔 {label} · 充电快档 {fast}）`
///   （「 · 充电快档」两侧有空格，逐字；注记用「充电快档」而非
///   「充电时」，错开双 15 相邻误读且对齐 stats.rs charging-fast 口径）。
///
/// {fast} = humanize_interval_secs(POLL_CHARGING)（core.rs 常量，单一
/// 真值，现渲染「15 秒」，快档常量未来调整时文案自动跟随）；
/// {label} = menu_poll_interval_label(poll_secs)（档外值自带
/// 「（非标准档）」）。实现红线：fast 是运行态节拍而非配置档
/// （15 ∉ POLL_TIERS_SECS），永不追加「（非标准档）」——误套
/// menu_poll_interval_label 即说谎。纯文本拼写，不持有业务判定
/// （三态由 `AppCore::fast_poll_state` 判定，守住 E23「只收文本
/// 拼写」边界）。
pub fn menu_refresh_label(poll_secs: u64, state: FastPollState) -> String {
    let label = menu_poll_interval_label(poll_secs);
    match state {
        FastPollState::Off => format!("立即刷新（当前间隔 {label}）"),
        FastPollState::Unified => {
            let fast = humanize_interval_secs(POLL_CHARGING.as_secs());
            format!("立即刷新（当前间隔 {fast}·充电中，常规 {label}）")
        }
        FastPollState::Mixed => {
            let fast = humanize_interval_secs(POLL_CHARGING.as_secs());
            format!("立即刷新（当前间隔 {label} · 充电快档 {fast}）")
        }
    }
}

/// 设置窗口轮询档位下拉项文案：人性化单位，默认档（D-66：1800s）追加「（默认）」。
/// 显示层专用；档位数值仍以秒经 `POLL_TIERS_SECS` 索引回写配置。
pub fn poll_tier_label(secs: u64) -> String {
    if secs == DEFAULT_POLL_SECS {
        format!("{}（默认）", humanize_interval_secs(secs))
    } else {
        humanize_interval_secs(secs)
    }
}

/// v0.3.22（D-87/D-88）：实验室 hint 终版文案——如实说明可逐项关闭、
/// 尚未完成全量真机验证、反馈渠道与生效方式（不承诺「零事故」）。
pub const LAB_HINT_TEXT: &str = "实验室（试验功能）：可逐项关闭；尚未完成全量真机验证，如遇异常请关闭对应功能并通过 Issues 反馈。改动保存后重启生效，可点『立即重启』立即应用。";
/// 四项有未保存改动时追加的行（D-88：改动保存后需重启方完全生效）。
pub const LAB_DIRTY_TEXT: &str = "已修改，重启后生效。";

/// 分档说明示例（v0.3.6 规范偏差修正：按当前档数/阈值实时拼写，替代静态常量）。
/// 例：thresholds = [20, 50] →「阈值 1–99 严格递增；电量 ≤ 数值归入对应档（20/50 → 0–20 / 21–50 / 51–100）」。
pub fn tier_hint_dynamic(thresholds: &[u8]) -> String {
    let ths = thresholds
        .iter()
        .map(|t| t.to_string())
        .collect::<Vec<_>>()
        .join("/");
    let mut parts: Vec<String> = Vec::new();
    let mut lo: i32 = 0;
    for &t in thresholds {
        parts.push(format!("{lo}–{t}"));
        lo = t as i32 + 1;
    }
    parts.push(format!("{lo}–100"));
    format!(
        "阈值 {FONT_TIER_THRESHOLD_MIN}–{FONT_TIER_THRESHOLD_MAX} 严格递增；电量 ≤ 数值归入对应档（{ths} → {}）",
        parts.join(" / ")
    )
}

/// 闸门复选框文案（D-87）：入口行用「精细化设置」交互语义 + 常驻计数。
pub fn fine_tune_gate_text(enabled: usize) -> String {
    format!("精细化设置（试验功能，已开启 {enabled}/4 项）")
}

/// 实验室 hint 动态拼写（D-88）：四项有未保存改动时追加「已修改，重启后生效」。
pub fn lab_hint_text(dirty: bool) -> String {
    if dirty {
        format!("{LAB_HINT_TEXT}\n{LAB_DIRTY_TEXT}")
    } else {
        LAB_HINT_TEXT.to_string()
    }
}

/// 「settings applied」日志行（v0.3.14，B9；E23 第 2 步从 tray::settings
/// 逐字平移）：列出发生变化的用户可感知键（阈值/闪烁/静音/通知/轮询/
/// 自启 + 外观键 + 实验项 + 精细化展开态）；无变化时明确标注。
/// 键集合与原函数逐一对应，增删键即行为变化。
pub fn settings_applied_note(prev: &Config, next: &Config) -> String {
    let mut parts: Vec<String> = Vec::new();
    macro_rules! cmp {
        ($field:ident) => {
            if prev.$field != next.$field {
                parts.push(format!(
                    "{} {:?}->{:?}",
                    stringify!($field),
                    prev.$field,
                    next.$field
                ));
            }
        };
    }
    cmp!(low_threshold);
    cmp!(blink_enabled);
    cmp!(notification_silent);
    cmp!(notifications_enabled);
    cmp!(poll_interval_secs);
    cmp!(autostart);
    cmp!(icon_shape);
    cmp!(icon_background);
    cmp!(icon_font_color);
    cmp!(icon_text_scale);
    cmp!(icon_bg_scale);
    cmp!(experimental_persist_last_known);
    cmp!(experimental_stats);
    cmp!(experimental_per_device_polling);
    cmp!(experimental_bluetooth_wired);
    cmp!(ui_fine_tune_expanded);
    if parts.is_empty() {
        "settings applied (no tracked key changed)".to_string()
    } else {
        format!("settings applied: {}", parts.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        fine_tune_gate_text, humanize_interval_secs, lab_hint_text, menu_poll_interval_label,
        menu_refresh_label, poll_tier_label, settings_applied_note, tier_hint_dynamic,
        LAB_HINT_TEXT,
    };
    use crate::config::{Config, DEFAULT_POLL_SECS, POLL_TIERS_SECS};
    use crate::core::FastPollState;
    use crate::core::POLL_CHARGING;

    /// 轮询间隔人性化换算（2026-10-25 用户反馈：秒不易读）——五档
    /// （D-66：300/600/1800/3600/7200）显示输出逐档断言 + 换算规则边界：
    /// 整除 3600 → 小时、整除 60 → 分钟、其余保留秒。
    #[test]
    fn humanize_interval_secs_spells_five_tiers() {
        assert_eq!(humanize_interval_secs(300), "5 分钟");
        assert_eq!(humanize_interval_secs(600), "10 分钟");
        assert_eq!(humanize_interval_secs(1800), "30 分钟");
        assert_eq!(humanize_interval_secs(3600), "1 小时");
        assert_eq!(humanize_interval_secs(7200), "2 小时");
    }

    /// 换算规则边界：整除 3600 优先于整除 60；非整除保留秒。
    #[test]
    fn humanize_interval_secs_unit_rule_edges() {
        assert_eq!(humanize_interval_secs(60), "1 分钟");
        assert_eq!(humanize_interval_secs(3660), "61 分钟");
        assert_eq!(humanize_interval_secs(15), "15 秒");
        assert_eq!(humanize_interval_secs(90), "90 秒");
        assert_eq!(humanize_interval_secs(1), "1 秒");
    }

    /// 设置窗口下拉项文案：五档逐档断言，默认档（1800s）追加「（默认）」。
    #[test]
    fn poll_tier_label_spells_five_tiers_with_default_mark() {
        assert_eq!(poll_tier_label(300), "5 分钟");
        assert_eq!(poll_tier_label(600), "10 分钟");
        assert_eq!(poll_tier_label(1800), "30 分钟（默认）");
        assert_eq!(poll_tier_label(3600), "1 小时");
        assert_eq!(poll_tier_label(7200), "2 小时");
        // 与档位表逐一对应：POLL_TIERS_SECS 顺序即下拉顺序。
        let labels: Vec<String> = POLL_TIERS_SECS
            .iter()
            .map(|&s| poll_tier_label(s))
            .collect();
        assert_eq!(
            labels,
            vec!["5 分钟", "10 分钟", "30 分钟（默认）", "1 小时", "2 小时"]
        );
        // 「（默认）」标注锚定 DEFAULT_POLL_SECS，不硬编码档位位置。
        assert!(poll_tier_label(DEFAULT_POLL_SECS).ends_with("（默认）"));
    }

    /// FR-7（RB2）验证：构造 poll_interval_secs=900（手改 config 的非档值），
    /// 菜单文案按配置真值显示并标注「（非标准档）」，与下拉文案指向同一
    /// 语义（真值 15 分钟，而非被吸附档位 10 分钟）；标准档不追加标注。
    #[test]
    fn menu_poll_interval_label_marks_non_standard_tier() {
        // 非档值 900s：真值 15 分钟 + 非标准档标注。
        assert_eq!(menu_poll_interval_label(900), "15 分钟（非标准档）");
        assert!(!POLL_TIERS_SECS.contains(&900));
        // 下拉档位表无 900 一项——菜单不再假装它等于某个档位。
        let dropdown_labels: Vec<String> = POLL_TIERS_SECS
            .iter()
            .map(|&s| poll_tier_label(s))
            .collect();
        assert!(!dropdown_labels.contains(&"15 分钟".to_string()));
        // 标准档五档不追加标注，文案与 humanize 一致。
        for &t in &POLL_TIERS_SECS {
            assert_eq!(menu_poll_interval_label(t), humanize_interval_secs(t));
        }
        // 其他非档值（非整除分钟）同样标注。
        assert_eq!(menu_poll_interval_label(90), "90 秒（非标准档）");
    }

    #[test]
    fn tier_hint_dynamic_spells_ranges() {
        assert_eq!(
            tier_hint_dynamic(&[20, 50]),
            "阈值 1–99 严格递增；电量 ≤ 数值归入对应档（20/50 → 0–20 / 21–50 / 51–100）"
        );
        assert_eq!(
            tier_hint_dynamic(&[10, 30, 50, 70]),
            "阈值 1–99 严格递增；电量 ≤ 数值归入对应档（10/30/50/70 → 0–10 / 11–30 / 31–50 / 51–70 / 71–100）"
        );
    }

    /// v0.3.22（D-87）：闸门计数常驻动态显示——N/4 实时拼写。
    #[test]
    fn fine_tune_gate_text_spells_count() {
        assert_eq!(
            fine_tune_gate_text(0),
            "精细化设置（试验功能，已开启 0/4 项）"
        );
        assert_eq!(
            fine_tune_gate_text(4),
            "精细化设置（试验功能，已开启 4/4 项）"
        );
        assert_eq!(
            fine_tune_gate_text(2),
            "精细化设置（试验功能，已开启 2/4 项）"
        );
    }

    /// v0.3.22（D-88）：hint 终版口径 + 未保存改动动态追加行。
    #[test]
    fn lab_hint_text_marks_dirty() {
        assert!(!lab_hint_text(false).contains("已修改，重启后生效"));
        assert!(lab_hint_text(true).contains("已修改，重启后生效"));
        assert!(lab_hint_text(false).contains("实验室（试验功能）"));
        assert!(LAB_HINT_TEXT.contains("尚未完成全量真机验证"));
    }

    /// E23 第 2 步行为钉板：settings_applied_note 的键集合与输出格式
    /// （debug 值对、", " 连接、无变化标注）与原 tray::settings 内
    /// 函数逐字一致。
    #[test]
    fn settings_applied_note_pins_tracked_keys_and_format() {
        let prev = Config::default();
        // 无变化：明确标注
        assert_eq!(
            settings_applied_note(&prev, &prev),
            "settings applied (no tracked key changed)"
        );
        // 单键变化：debug 值对格式
        let mut next = prev.clone();
        next.low_threshold = 25;
        assert_eq!(
            settings_applied_note(&prev, &next),
            "settings applied: low_threshold 20->25"
        );
        // 多键变化：", " 连接、键序与 cmp! 宏展开序一致
        let mut next2 = prev.clone();
        next2.low_threshold = 25;
        next2.autostart = !prev.autostart;
        let note = settings_applied_note(&prev, &next2);
        assert!(note.starts_with("settings applied: low_threshold 20->25, autostart "));
        // 未追踪键（如阈值数组）变化不产生输出行——钉住「追踪键集合」边界
        let mut next3 = prev.clone();
        next3.icon_font_tier_thresholds = vec![15, 45];
        assert_eq!(
            settings_applied_note(&prev, &next3),
            "settings applied (no tracked key changed)"
        );
    }

    /// 充电三态「立即刷新」菜单文案逐字断言（签发稿第二节六形；标准档
    /// 1800s）。「15 秒」由 POLL_CHARGING 渲染、「30 分钟」由
    /// menu_poll_interval_label 渲染——单一真值，两侧互不越界。
    #[test]
    fn menu_refresh_label_spells_standard_tier_three_states() {
        // 态一·无快档：与现状基线逐字一致，零变化。
        assert_eq!(
            menu_refresh_label(1800, FastPollState::Off),
            "立即刷新（当前间隔 30 分钟）"
        );
        // 态二·统一快档：「15 秒·充电中」中无空格。
        assert_eq!(
            menu_refresh_label(1800, FastPollState::Unified),
            "立即刷新（当前间隔 15 秒·充电中，常规 30 分钟）"
        );
        // 态三·混合：「 · 充电快档」两侧有空格。
        assert_eq!(
            menu_refresh_label(1800, FastPollState::Mixed),
            "立即刷新（当前间隔 30 分钟 · 充电快档 15 秒）"
        );
        // 分口径（B 找茬 F1 遗留断言）：Off 态主值槽位与
        // menu_poll_interval_label(poll_secs) 逐字一致。
        assert_eq!(
            menu_refresh_label(1800, FastPollState::Off),
            format!("立即刷新（当前间隔 {}）", menu_poll_interval_label(1800))
        );
        // 分口径：Unified 态「常规 …」段与 menu_poll_interval_label(1800)
        // 逐字一致。
        assert_eq!(
            menu_refresh_label(1800, FastPollState::Unified),
            format!(
                "立即刷新（当前间隔 {}·充电中，常规 {}）",
                humanize_interval_secs(POLL_CHARGING.as_secs()),
                menu_poll_interval_label(1800)
            )
        );
        // 验收红线（开关关闭回退）：Off 态文案不含「充电」字样。
        assert!(!menu_refresh_label(1800, FastPollState::Off).contains('充'));
    }

    /// 档外值（900s）三形：FR-7 规则原位生效——(900, Off) 与现行基线
    /// 逐字相同，作 FR-7 回归锚；(900, Unified/Mixed) 档外标注随各自
    /// 槽位原位生效；负向断言「15 秒永不出现（非标准档）」。
    #[test]
    fn menu_refresh_label_non_standard_tier_forms_and_fast_redline() {
        // (900, Off)：FR-7 回归锚——与现行基线渲染路径逐字相同。
        assert_eq!(
            menu_refresh_label(900, FastPollState::Off),
            "立即刷新（当前间隔 15 分钟（非标准档））"
        );
        assert_eq!(
            menu_refresh_label(900, FastPollState::Off),
            format!("立即刷新（当前间隔 {}）", menu_poll_interval_label(900))
        );
        // (900, Unified)：档外标注随「常规 …」右注原位生效。
        assert_eq!(
            menu_refresh_label(900, FastPollState::Unified),
            "立即刷新（当前间隔 15 秒·充电中，常规 15 分钟（非标准档））"
        );
        // 分口径：Unified 态「常规 …」段与 menu_poll_interval_label(900)
        // 逐字一致（档外值含「（非标准档）」）。
        assert_eq!(
            menu_refresh_label(900, FastPollState::Unified),
            format!(
                "立即刷新（当前间隔 {}·充电中，常规 {}）",
                humanize_interval_secs(POLL_CHARGING.as_secs()),
                menu_poll_interval_label(900)
            )
        );
        // (900, Mixed)：档外标注随主值槽位原位生效。
        assert_eq!(
            menu_refresh_label(900, FastPollState::Mixed),
            "立即刷新（当前间隔 15 分钟（非标准档） · 充电快档 15 秒）"
        );
        // 实现红线（负向断言）：fast 值（15 秒）是运行态节拍而非配置档
        //（15 ∉ POLL_TIERS_SECS），永不追加「（非标准档）」——三态 ×
        // 标准档/档外值均不得出现「15 秒（非标准档）」。
        assert!(!POLL_TIERS_SECS.contains(&POLL_CHARGING.as_secs()));
        for p in [1800, 900] {
            for s in [
                FastPollState::Off,
                FastPollState::Unified,
                FastPollState::Mixed,
            ] {
                assert!(
                    !menu_refresh_label(p, s).contains(&format!(
                        "{}（非标准档）",
                        humanize_interval_secs(POLL_CHARGING.as_secs())
                    )),
                    "fast 值永不追加「（非标准档）」：p={p}, state={s:?}"
                );
            }
        }
    }
}
