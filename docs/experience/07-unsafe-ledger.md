# unsafe 台账（红蓝对抗改进项 E9）

> 建立：2026-10-24（红蓝对抗 P1a 窗口，实施方）。
> 红线：**只登记不改码**——本次仅逐块补 `// SAFETY(E9): …` 不变量注释并建立
> 本台账，未改动任何 unsafe 代码本身（核验：插入前后各文件 `grep -c unsafe`
> 计数不变，新增行全部为有 `SAFETY(E9)` 标记的注释行）。
> 注：本台账的「E9」指红蓝对抗终审报告改进项编号（unsafe 台账），与经验条目
> 编号 E9（质量门禁）无关。

## 计数口径

- 统计方式：`grep -rna 'unsafe' volttray/crates/*/src/*.rs`，人工剔除非代码命中
  （注释 / 文档中的 "unsafe" 字样，如 `autostart.rs` 内以「实现说明」开头的说明性 doc 注释）。
  `-a` 不可省：**判定依赖读路径 / FS 语义，先换 FS 复判**（G-31 统一口径，2026-10-31；与 `ci.yml`「src files must not be judged binary (E1)」注释同源）——2026-10-24 字节级实测：源文件本身干净（满读 md5 与远端 blob 一致、无短读），根因为本地沙箱网络盘 **SEEK_HOLE 语义**所致的读路径伪影（grep 探测读命中稀疏空洞段而误判二进制）；此前「main.rs 含会被 grep 误判为二进制的字节序列」的说法即此伪影，不归因于「文件含多字节字符」。发现判二进制先换文件系统复判，复判一致才按真实字节污染处置。
- 剔除步骤已固化为命令（不靠人工记忆、不靠行号记忆）：
  `grep -rna 'unsafe' volttray/crates/*/src/*.rs | grep -vE ':[0-9]+:[[:space:]]*(//|/\*)' | wc -l`
  —— 过滤掉整行以 `//` 或 `/*` 开头的注释行（SAFETY 注释与说明性 doc 注释）；
  2026-10-28 实测＝96，与下表逐文件小计逐行一致（settings 46 / main 11 / statsexport 10 / export 6 / rawinput 5 / bootlog 5 / autostart 5 / icon 3 / single 2 / aumid 2 / timelocal 1）。
- 实测总数 **96 处**＝69 个 `unsafe {}` 块 ＋ 18 个 `unsafe fn` ＋ 9 个
  `unsafe extern "system" fn`（后者为 windows crate 回调签名要求）。
- 口径勘误（2026-10-28 红蓝对抗局复核）：终审报告口径「90 处」为**旧口径**，
  以台账最新实测（当前 96）为准；差异未逐条回溯（报告统计时点与当时基线之间
  可能存在代码漂移），作为已知差异披露，不影响逐块登记的完整性。
- 变更注记（2026-10-25 复核）：main.rs / single.rs 重构净减、export.rs 新增入册，
  实测总数 97→96，本表已同步；以最新实测为准。
- 机器可核验：全仓 `SAFETY(E9)` 标记数必须等于下表合计（96）。精确核验命令：
  `grep -ra 'SAFETY(E9)' volttray/crates | wc -l`（2026-10-26 实测＝96）；字面「全仓 grep」
  会把经验库文档自身引用计入（本台账 5 处、[06](06-release-reconciliation.md) 引用 1 处，合计 6 处，得 102）而误判漂移，故范围必须限定 `volttray/crates`。
  本命令固化进 [06](06-release-reconciliation.md) F.1 发布对账复核项引用。任一处新增 /
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
  指针生命周期 / 句柄有效性 / 对齐与容量 / 所有权与线程契约。注释属于整个
  item：注释与 `fn` 之间允许隔 attribute 行（唯一既有例外登记：`settings.rs` `create_ctl`）。
- 对「转调已注释函数」的调用点（如 settings.rs 中 edit_text /
  handle_color_pick 的调用点），注释指向被调函数的不变量，不重复展开。
- 注释是登记而非背书：后续改动若破坏不变量，先更新注释再改码，评审时对账。

## 发布对账

- 复核项登记于 [06-release-reconciliation.md](06-release-reconciliation.md)
  F.1「评审 / 登记项」：每次发布对账核对实测 unsafe 数与本台账一致，
  不一致即打回。
