# v0.3.27 三缺陷修复设计（方案 → 代码方案 → 落实）

> 状态：v0.3.27 版随版设计记录。三步各自做过一轮红队（找漏洞/找反例）与蓝队（反驳并给出更强约束）。
> 约束前提：不新增第三方依赖；不改 release profile；固定菜单项数量不增加（M13 已超限，见对账表 F.2）；
> 所有改动保持 D-33（休眠零查询）与「菜单/文案改动须 README 双语同步」（E44）。

## 0. 三个缺陷的证伪链（先证根因，再谈方案）

| 编号 | 用户陈述 | 代码事实（行号为 v0.3.26 基线） | 根因判定 |
|---|---|---|---|
| B1 | 离线很久的设备一直显示、删不掉 | `crates/tray/src/menu.rs:155` 每台设备一行 `CheckMenuItem`；离线/虚拟行只做 pin，没有删除入口。`crates/app/src/persist.rs:393` 的 `LastKnownStore::remove()` 全仓无生产调用点（死代码）。`persist.rs:611` 的 `enforce_caps` 只按「条数 > 32」与「字节 > 256KB」淘汰，注释明确「不做时间过期淘汰」 | 缺三件东西：时间过期、持久化删除的脏标记、菜单删除入口 |
| B2 | 没手动钉选时，图标偶尔跳到另一台设备 | `crates/app/src/core.rs:1181` `default_display_index` 每次快照按「未充电中电量最低」重算；`core.rs:1216` 每个 `snapshot()` 都重算一次；`crates/tray/src/main.rs:3838` 图标取 `pinned`，无 pin 时用该下标 | 每次重算 + 无滞回，两台电量交叉（或 1% 抖动）即换图标 |
| B3 | 报告不直观、有些图连纵坐标都没有 | `crates/app/src/attribution.rs:1880` 日耗电柱只画一条基线（`:1893`），无 y 轴刻度、无数值标签、无单位；`:1869` 的 24h 时间轴是 14px 色条无刻度；全页 6 个区块全量铺开，无「简单看」入口 | 缺 y 轴 + 缺默认简洁视图 |

## 1. 方案（做什么，不做什么）

### 1.1 B1 方案：三件套「过期 + 可删 + 删得干净」

1. **时间过期**：只对「虚拟条目（离线最后已知电量）」生效，超过 `forget_offline_days`（新配置键，默认 30，`0` = 永不自动遗忘）的条目自动遗忘。真机在线条目不参与。
2. **菜单可删**：离线/休眠/虚拟设备行改为子菜单：「显示此设备电量（钉选）」+「忘记此设备…」，点击后 `MessageBoxW(MB_YESNO)` 二次确认。真机在线行保持原样（单击即钉选，不动既有肌肉记忆）。
3. **删得干净**：`LastKnownStore` 增加 `purge_dirty` 标记，`remove()` 置位并纳入 `flush_if_due` / `shutdown_flush` 的落盘条件——否则删了内存记录但文件没重写，重启后设备复活（这是现行 `remove()` 从未被调用的隐性原因之一）。

红队（R1）：默认 30 天会自动删用户数据，属于「静默数据丢失」。用户可能希望保留一年前的记录。
蓝队（B1）：① 默认值写进 `config.toml` 并带注释，用户可见可改；② 只删「离线最后已知」这一条本就会过期失真的展示项，不删 `stats.jsonl` 历史统计；③ 删除动作全部落 `bootlog`（`forget device: <key> reason=expired|user`），可追溯；④ UI 侧删前有确认框。
红队（R2）：子菜单会让「点一下换图标」变成两次点击，破坏 `menu.rs` 顶部注释的结构，也让 `MenuIds.device_rows` 语义变混。
蓝队（B2）：只有「无实时读数」的行改成子菜单（这类行本来就不该被钉成主展示），有读数的行保持单击钉选；`device_rows` 只存「钉选项」的 id，新增 `device_forget_rows` 单独存忘记项，主循环按两个表分派，`t07_fixed_item_count_is_nine` 用空设备表构造，计数不受影响。
红队（R3）：`remove()` 里新增 `purge_dirty` 会不会让 30s/60s 防抖失效、写盘风暴？
蓝队（B3）：`purge_dirty` 是布尔，只在真正删除时置位，落盘后清掉；`flush_if_due` 仍受 `global_debounce`（10s）节流，单次删除最多触发一次重写。
红队（R4）：过期判定用 `persisted_last_seen`，时钟回拨会让设备瞬间「过期」。
蓝队（B4）：过期判定取 `now.saturating_sub(last_seen) >= days`，回拨时差值为 0 → 不删；这是保守方向（不误删）。

### 1.2 B2 方案：滞回（hysteresis）替代每 tick 重算

`AppCore` 增加 `auto_shown: Option<String>`（当前自动展示的设备 id）。选择算法改为：

1. 候选集不变（`eligible` = `Sleeping` / `Online` / 未灰显；优先未充电）；取「候选最低电量」`challenger`。
2. 若 `auto_shown` 指向的设备**仍存在且仍有读数且 eligible**：只有当 `challenger.percent + AUTO_SWITCH_MARGIN <= current.percent`（默认 3 个百分点）才切换，否则维持不动。
3. 否则（当前设备消失/读数丢失/不再 eligible）立刻切换到 `challenger`。
4. 切换后写入 `auto_shown`；`remove_device` / `forget_device` 会清空它。

红队（R5）：滞回会掩盖「另一台掉到 5%」的告警——用户可能永远看不到真正快没电的那台。
蓝队（B5）：低电本身有独立通知链（`lowbats` + toast + 闪烁），不依赖图标展示位；且滞回只有 3 个百分点，电量真掉下去必然越过分隔。若用户要旧语义，可用 `AUTO_SWITCH_MARGIN=0` 的常量改动（决策记录里写明）。
红队（R6）：`auto_shown` 是内存态，重启后仍会「跳」。
蓝队（B6）：重启后没有「之前的展示」概念，挑最低电量是正确初值；用户痛点发生在运行期抖动。另外缓存 `auto_shown` 到磁盘属于新增持久化面，风险大于收益，明确不做。
红队（R7）：测试强耦合 `default_display_index(&self)` 的 `&self` 语义，改 `&mut self` 会不会波及快照借用？
蓝队（B7）：`snapshot(&mut self)` 内先算 `default_device` 再读 `self.config()`，借用已分离；全仓 `default_display_index` 调用点只有 `snapshot` 与 5 处单测（都是 `let mut core`），改签名可控。

### 1.3 B3 方案：默认「简洁视图」+ 全部柱图补纵坐标

1. **视图切换**：页首加「简洁视图 / 完整视图」两个按钮，默认简洁；`#full`（分日视图、静止 vs 活动、趋势佐证区、证据与口径区、口径表）默认 `display:none`。简洁区只保留：每设备一句话结论 + 当前电量横条（0–100 轴）+ 近 N 天日耗电柱（带 y 轴）。
2. **日耗电柱补纵坐标**：y 轴刻度（0 / 中值 / 最大值，单位 %）+ 3 条网格线 + 柱数值标签（柱数 ≤ 31 时全标，否则只标最大柱与悬停 title）+ 「纵轴：每日耗电 %」说明。
3. **24h 时间轴补刻度**：0/6/12/18/24 小时刻度文字。
4. **堆叠横条补总量**：`#c3` 每行右侧已有百分比串，补一行「总时长」文本（可选，低优先）。

红队（R8）：默认隐藏内容 = 用户找不到「详细数据」，属于功能降级。
蓝队（B8）：切换按钮在首屏顶部、默认态明确高亮；`完整视图` 一键展开；不删任何字段、不删任何表格行——只是默认折叠。
红队（R9）：`html_self_contained_no_external_requests` 断言禁用 `url(`、`<img`、`fetch(`，加内联 SVG/CSS 会不会踩线？
蓝队（B9）：网格线与刻度都用 `createElementNS` 的 `rect`/`text`，不引入任何 URL；视图切换用 `element.style.display`，无外部请求。新增守门测试反向断言「HTML 不含 `display:none` 之外的隐藏技巧」不必要，改为正向断言「含纵轴文案 + 含简洁视图按钮」。
红队（R10）：`g28` 等既有断言依赖模板字符串，改动模板可能误伤（例如把 `更新于 ` 写进被删区块）。
蓝队（B10）：只做「包一层 + 追加」，不删既有字符串；落地后跑全量 `cargo test --workspace --locked` 验证 82 项 tray + 各 crate 单测。

## 2. 代码方案（落点清单）

| 文件 | 改动 |
|---|---|
| `crates/app/src/config.rs` | 新增 `forget_offline_days: u64`（默认 30，钳 0..=3650）：struct 字段 + `Default` + `parse` 分支 + `to_toml` 模板 + 单测 |
| `crates/app/src/persist.rs` | 新增 `purge_dirty: bool`；`remove()` 置位；`flush_if_due`/`shutdown_flush` 条件纳入；新增单测「remove 后必须重写文件、重启不再出现」 |
| `crates/app/src/core.rs` | `auto_shown: Option<String>` + `AUTO_SWITCH_MARGIN`；`default_display_index(&mut self)` 滞回；`forget_device(&mut self, id) -> Option<String>`；`stale_virtual_ids(&self, wall_now, max_age_secs)`；新增单测 |
| `crates/tray/src/menu.rs` | 离线/休眠/虚拟行改 `Submenu`（钉选项 + 忘记项）；`MenuIds` 加 `device_forget_rows`；新增单测 |
| `crates/tray/src/main.rs` | 菜单事件分派忘记项（MB_YESNO 确认 → `forget_device` + `store.remove` + `refresh_ui` + bootlog）；IO 循环每轮过期清扫；`menu_content_sig` 加入「行类型」以免子菜单/勾选态不同步 |
| `crates/app/src/attribution.rs` | 模板：页首视图切换 + `#simple`/`#full` 容器；柱图 y 轴/网格/数值标签；时间轴小时刻度；新增守门单测 |
| `volttray/Cargo.toml` + `Cargo.lock` | workspace version 0.3.26 → 0.3.27 |
| `README.md` / `README.en-US.md` / `RELEASES.md` / `docs/HANDOVER.md` / `volttray/docs/decisions.md` | 用户可见面同步（E44）：菜单新入口、新配置键、报告默认视图；决策追加 D-100… |

## 3. 落实与验收

- 编译与测试：`cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --locked -- -D warnings`、`cargo test --workspace --locked`。
- 新增单测覆盖：B1（过期清扫、忘记流程、purge 落盘）、B2（滞回不跳、真离线切换）、B3（纵轴文案、简洁视图默认、无外部请求）。
- 未验证边界：真机 HID++ 双设备交叉抖动无法在本机复现，仅以单测构造；HTML 视觉需人工开浏览器核对。
