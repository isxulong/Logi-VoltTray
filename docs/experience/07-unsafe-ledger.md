# 07 · unsafe 台账（红蓝对抗改进项 E9）

> 建立：2026-10-24（红蓝对抗 P1a 窗口，实施方）。
> 红线：**只登记不改码**——本次仅逐块补 `// SAFETY(E9): …` 不变量注释并建立
> 本台账，未改动任何 unsafe 代码本身（核验：插入前后各文件 `grep -c unsafe`
> 计数不变，新增行全部为有 `SAFETY(E9)` 标记的注释行）。
> 注：本台账的「E9」指红蓝对抗终审报告改进项编号（unsafe 台账），与经验条目
> 编号 E9（质量门禁）无关。

## 计数口径

- 统计方式：`grep -rn 'unsafe' volttray/crates/*/src/*.rs`，人工剔除非代码命中
  （注释 / 文档中的 "unsafe" 字样，如 `autostart.rs` L278–279 的说明性 doc 注释）。
- 实测总数 **96 处**＝69 个 `unsafe {}` 块 ＋ 18 个 `unsafe fn` ＋ 9 个
  `unsafe extern "system" fn`（后者为 windows crate 回调签名要求）。
- 终审报告口径为「90 处」，台账初建（2026-10-24）实测 97，差 7；差异未
  逐条回溯（报告统计时点与当时基线之间可能存在代码漂移），作为已知差异
  披露，不影响逐块登记的完整性。
- 变更注记（2026-10-25 复核）：main.rs / single.rs 重构净减、export.rs 新增入册，
  实测总数 97→96，本表已同步；以最新实测为准。
- 机器可核验：全仓 `SAFETY(E9)` 标记数必须等于下表合计（96）。任一处新增 /
  删除 unsafe 代码，必须同批增删对应 SAFETY 注释并更新本台账。

## 分布（按文件）

| 文件 | unsafe {} 块 | unsafe fn | unsafe extern fn | 小计 |
| --- | --- | --- | --- | --- |
| crates/app/src/aumid.rs | 2 | 0 | 0 | 2 |
| crates/app/src/autostart.rs | 5 | 0 | 0 | 5 |
| crates/tray/src/bootlog.rs | 5 | 0 | 0 | 5 |
| crates/tray/src/export.rs | 6 | 0 | 0 | 6 |
| crates/tray/src/icon.rs | 3 | 0 | 0 | 3 |
| crates/tray/src/main.rs | 11 | 0 | 0 | 11 |
| crates/tray/src/rawinput.rs | 4 | 0 | 1 | 5 |
| crates/tray/src/settings.rs | 23 | 18 | 5 | 46 |
| crates/tray/src/single.rs | 2 | 0 | 0 | 2 |
| crates/tray/src/statsexport.rs | 7 | 0 | 3 | 10 |
| crates/tray/src/timelocal.rs | 1 | 0 | 0 | 1 |
| **合计** | **69** | **18** | **9** | **96** |

## 注释约定

- 每处 unsafe 正上方一行 `// SAFETY(E9): <不变量>`，内容为该处成立所依赖的
  指针生命周期 / 句柄有效性 / 对齐与容量 / 所有权与线程契约。
- 对「转调已注释函数」的调用点（如 settings.rs 中 edit_text /
  handle_color_pick 的调用点），注释指向被调函数的不变量，不重复展开。
- 注释是登记而非背书：后续改动若破坏不变量，先更新注释再改码，评审时对账。

## 发布对账

- 复核项登记于 [06-release-reconciliation.md](06-release-reconciliation.md)
  F.1「评审 / 登记项」：每次发布对账核对实测 unsafe 数与本台账一致，
  不一致即打回。
