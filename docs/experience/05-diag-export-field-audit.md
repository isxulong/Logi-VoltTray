# 诊断导出字段审计（T-02b）

> 定位：P1 · 审查文档 + 可回归测试。审计对象＝诊断导出 JSON（「导出诊断信息…」菜单产生的文件）的全部序列化字段，逐字段登记内容来源 / 泄露面评估 / 已覆盖的 redact 规则。三处必查意外泄露点专节见第三节。
>
> 审计基线：分支 `adversarial-impl-2026-10`，代码位置以审计当轮 HEAD 为准——`volttray/crates/app/src/diag_export.rs`（结构体定义）、`volttray/crates/app/src/redact.rs`（文本侧脱敏）、`volttray/crates/tray/src/main.rs`（export_diag 组装与 ChannelEnumResult 构造）、`volttray/crates/transport/src/hid_impl.rs`（tag/key/错误文本来源）、`volttray/crates/transport/src/events.rs`（DiagKind 事件形态）。
> 容量与代码位置快照时点：本文容量常数与行号锚点为 2026-10-26 修订批核验时点快照（该时点 DIAG_CAPACITY 已扩容至 2048、LOG_TAIL_MAX_LINES 已 4096、export_diag 组装已迁 export.rs）；后续以常量符号引用为准、行号会漂移。本快照时点纳入 06 F.1 发布对账项复核。
>
> 结论由代码实证得出，非预填：每条「已覆盖」都给出机制依据，「未覆盖」给出处置结论。审计结论的可回归性由 `diag_export.rs` 测试 `t02b_all_fields_enumerated_and_no_user_path_leak` 守护（见第五节）——结构体字段集漂移时测试失败，强制同步本文档。

## 一、脱敏机制总述（审计的前提）

导出走 `DiagExport::to_json_string_with(&device_names)`：先把**整个结构体** `serde_json::to_string_pretty` 序列化为 JSON 文本，再对**全文**执行 `redact::redact_text`（T-02a，`redact.rs`）。因此两条规则都是**字段无关的全文级**：

- 规则一：`C:\Users\<段>` 的 `<段>` → `<USER>`，JSON 转义形态（`C:\\Users\\`）与原始形态（`C:\Users\`）都识别——任何字段值里出现该形态都会被处理，**不依赖字段名单**；
- 规则二：设备名按收集顺序 → 「设备N」。设备名由 tray 侧 `export_diag` 在同一次持锁内从事件流 `DiagKind::DeviceFound` 收集（现于 `export.rs`，DeviceFound 收集段 2026-10-26 时点为 L256–263；原锚点「`main.rs` L3344 起」已随组装迁移失效，后续以符号引用 `export.rs` `export_diag` 为准）；锁中毒时收集为空数组，仅规则一生效（T-02a 已登记残余，见第四节）。

另外 `devices` 字段（事件流）在序列化前还有**字段级**帧脱敏：`observer::export_value(true)` 对 `FrameTx`/`FrameRx` 的 `hex` 按 features.json 的 `sensitive_offsets` 遮蔽（`mask_event`，P1-6）。

## 二、DiagExport 逐字段审计

结构体定义以符号引用「`diag_export.rs` `DiagExport` 结构体定义起」为准（行号随重构漂移、不逐版维护；2026-10-26 时点为 L111 起）。20 个序列化字段（含 `skip_serializing_if` 可选字段；E22 新增 `wake_recovery_via_reenum`）。字段数以 `diag_export.rs` 测试 `t02b_all_fields_enumerated_and_no_user_path_leak` 的 expected 数组为准——2026-10-26 实测结构体 pub 字段与该数组均为 20，本文原「19」系漂移，此注即计数修订留痕：

| 字段 | 内容来源 | 泄露面评估 | redact 覆盖 |
| --- | --- | --- | --- |
| `devices` | `observer.export_value(true)`：环形缓冲 `DiagEvent` 的 JSON 数组，容量以 `observer.rs` `DIAG_CAPACITY` 为准（2026-10-26 时点为 2048；原「256 条」为漂移前值，此注即计数修订留痕）；导出入口为 `export.rs` `export_diag` 组装函数（2026-10-26 时点 L249 起，DeviceFound 收集 L256–263；原锚点「`main.rs` L3344」已随组装迁移失效） | 事件含设备名（`DeviceFound.name`）与帧 hex；hex 已按 sensitive_offsets 遮蔽；事件无路径字段 | 规则二覆盖设备名；无路径面 |
| `devices_reason`（skip） | 固定值 `"observer lock poisoned"` | 无个人信息 | 无需覆盖 |
| `log_tail.path` | `read_log_tail` 的 `path.to_string_lossy()`：bootlog 实际落点（APPDATA → EXE 目录 → TEMP 逐级兜底后的路径） | **含 `C:\Users\<用户名>` 主干**（APPDATA 前缀） | 规则一（双形态）覆盖 |
| `log_tail.lines` | bootlog 尾部，行数上限以 `diag_export.rs` `LOG_TAIL_MAX_LINES` 为准（2026-10-26 时点为 4096） | 设置类日志；`persist {key} to {路径} failed` 等模板可能含 APPDATA 路径（必查点三，见第三节） | 规则一覆盖路径形态；设备名见第三节残余分析 |
| `log_tail.reason` | `format!("log read failed: {e}")` / `"log file is empty"` | `std::io::Error` 的 Display 不含路径（os error 消息） | 无需覆盖（保守起见路径形态也已由规则一兜底） |
| `last_enum`（Option，null 或快照） | `enum_snap` 内存快照克隆 | 嵌套结构见下表 | 见下表 |
| `last_enum_reason`（skip） | 固定值 `"enum snapshot lock poisoned"` | 无个人信息 | 无需覆盖 |
| `channel_map_miss` / `frame_tx` / `frame_rx` | 原子计数器 | 纯数值 | 无需覆盖 |
| `avg_tx_interval_ms`（skip） | 帧间隔均值 | 纯数值 | 无需覆盖 |
| `feature_index_tx` / `feature_index_rx` | `FrameStats`：键为 `format!("0x{f:02X}")`（HID++ 帧第 3 字节格式化；构造点以符号引用「`main.rs` feature 统计格式化处」为准——行号锚点随重构漂移，不逐版维护） | 键是协议功能索引号，非用户数据 | 无需覆盖 |
| `arm_all_fills` / `wake_successes` / `wake_miss_no_path` / `wake_miss_zero_match` / `wake_miss_no_fingerprint` / `wake_miss_ambiguous` / `wake_recovery_via_reenum`（E22 新增：唤醒信号窗口内经整轮重枚举恢复注册计数） | 原子计数器 | 纯数值 | 无需覆盖 |
| `exported_at` | unix 秒 | 时间戳，无个人标识 | 无需覆盖 |
| `app_version` | `CARGO_PKG_VERSION` | 版本号 | 无需覆盖 |

嵌套结构：

| 结构 / 字段 | 内容来源 | 泄露面评估 | redact 覆盖 |
| --- | --- | --- | --- |
| `EnumSnapshot.at_ms` / `forced` / `candidate_channels` / `registered_slots` | 枚举时刻数值 | 无 | 无需覆盖 |
| `EnumSnapshot.error`（skip） | 枚举过程错误 `e.to_string()`（hidapi init/枚举错误原文） | 错误原文可能含设备路径 → 形态上属 `C:\Users\<段>` 即被规则一全文覆盖；hidapi Windows 错误文本一般不含用户名 | 规则一兜底覆盖 |
| `ChannelEnumResult.key` | `format!("{:04X}|{}", pid, tag)`（`hid_impl.rs` `ChannelInfo::key()`） | 实例指纹段（同 tag），无用户名/路径/设备名 | 不命中两规则；维持现状（同 tag 处置） |
| `ChannelEnumResult.pid` | `format!("0x{:04X}", pid)` | 硬件 PID | 无需覆盖 |
| `ChannelEnumResult.tag` | `path_tag(d.path())`：HID 实例路径 `#` 分隔第二段去 `&ColNN` 后缀（`hid_impl.rs` L164-174） | **必查点一**，见第三节 | 不命中两规则（形态无 `C:\Users`、非设备名）；处置见第三节 |
| `ChannelEnumResult.open` | 固定枚举：`"ok"` / `"failed"` / `"cached"` / `"reopened"` / `"cached-reopen-failed"`（`main.rs` 事件循环内三处构造点，行号以当版代码为准） | 无 | 无需覆盖 |
| `ChannelEnumResult.error`（skip） | hid open 失败 `e.to_string()`（classify_io 后 `IoError::Other` 保留原文，`main.rs` L1246） | **必查点二**，见第三节 | 规则一兜底覆盖（路径形态） |
| `DiagEvent` 各变体（`devices` 内） | `events.rs`：`id` = `format!("{}#{}", info.key(), slot)`（`main.rs` `info.key()` / `slot` 拼接处，行号以当版代码为准） | `id` 形态 `046D\|7&31a4c5e6&0&0000#3`——pid|tag#slot，实例指纹（同 tag 处置）；`name` 设备名（规则二）；`IoError.message` / `BatteryUnknown.detail` / `Alert.message` / `Info.message` 错误与提示原文（路径形态由规则一兜底）；帧 hex 已 mask | 规则二覆盖 `name`；规则一兜底 message 类字段 |

## 三、三处必查泄露点（实施令点名）

**① `ChannelEnumResult.tag`（BTHHID 蓝牙实例 ID 形态）**

BTHHID 路径实证（`wakechannel.rs` `bluetooth_path_systematically_misses` 测试内的 BTHHID 路径常量）：`\\?\BTHHID#{00001124-0000-1000-8000-00805f9b34fb}_VID&0002046d_PID_b01a#9&1c2d3e4f&0&0000#{4d1e55b2-...}`——`path_tag` 取 `#` 分隔第二段，即 `9&1c2d3e4f&0&0000`。USB 形态同构：`7&DEADBEEF&0&0001`。

评估：不含用户名、不含文件路径、不含设备产品名；是 Windows 生成的设备实例 ID（同设备跨导出稳定，**可作设备指纹**）。敏感度等同 VID/PID 序列号级——用于排障的核心信息（通道 key 配对、同型号去重、BTHHID 降级判断均依赖）。

处置结论：**不追加 redact 规则**。理由：泄露面为低敏感硬件指纹而非个人身份信息；若未来裁决要求模糊化，必须对 `tag` 与含 tag 原文的 `key`/`id`（`devices` 事件内）**三处同步**处理，违反「只追加、不删改既有语义」的本期边界——登记为可选后续项，进 06 文档（06-release-reconciliation.md）F 节对账滚动。

**② `ChannelEnumResult.error`（hid open 错误原文可能含路径）**

来源：`HidDeviceChannel::open` 的 `e.to_string()` 经 `classify_io`（`hid_impl.rs` L45/L73），`IoError::Other` 保留原文；`EnumSnapshot.error` 同机制。hidapi Windows 错误文本无法静态排除含路径的可能。

覆盖判定：**已覆盖**。redact 发生在序列化后的全文上（第一节），错误文本中任何 `C:\Users\<段>` 形态（无论 JSON 转义或原始）都被规则一替换——`redact.rs` 测试 `redacts_username_in_raw_paths` 即为此形态的等价断言；含设备名则被规则二覆盖。非路径错误文本（`"device not connected"` 类）不含个人信息。

**③ `log_tail.lines`（设置类日志）**

覆盖判定：**路径形态已覆盖**——bootlog 全部消息模板本轮逐一核验（`main.rs` 中 `bootlog::note` 调用共 88 处，含 `format!` 模板、多行构造与常量字符串；原稿误写「22 个 `format!` 模板 + 10 个常量字符串 = 32 处」，R1 评审更正，88 为批⑤合入时 `rg -c 'bootlog::note'` 对 main 现状实测——该数为时点快照；复核请重跑 `rg -c 'bootlog::note' volttray/crates/tray/src/main.rs`（2026-10-26 修订批实测：main.rs 101 处、全 crates 118 处，此注即计数漂移留痕）），含路径的模板两处：`persist {key} to {} failed: {e}`（settings 写回失败，目标为 APPDATA 下配置文件）与 `default config persist to {} failed: {e2}`（默认配置落盘失败，`main.rs` 日志模板 `default config persist to {} failed: {e2}`；原稿漏数此模板，R1 评审更正），均命中规则一；其余模板为状态码/计数/错误消息，不含用户名。**bootlog 当前不记录设备名**（模板核验结论）。

残余（F-A2 裁决遗留，诚实登记）：observer 锁中毒时 `device_names` 收集为空、规则二失效。该残余在 lines 侧的**当前实际暴露为零**（bootlog 模板不含设备名），但**未来防线**存在缺口：若后续新增记录设备名的 `bootlog::note` 调用，该设备名不受规则二保护（锁中毒场景下）。处置：①不硬堵（F-A2 裁决：真正兜底是本审计）；②本节作为该缺口的登记位——后续新增 bootlog 消息模板若含设备名/用户名，须回本表补行并评估是否追加规则；③第五节回归测试对「字段集漂移」的强制同步不覆盖「消息模板漂移」，后者依赖本登记与人工对账。

## 四、残余与处置结论汇总

| 残余 | 风险 | 处置 |
| --- | --- | --- |
| 锁中毒时规则二失效（T-02a 已登记，F-A2 裁决不硬堵） | events 侧：锁中毒时 `devices` 本身降级为空数组，无设备名可泄；lines 侧：当前模板无设备名，实际暴露为零 | 本审计第三节③为登记位；对账表（06 文档 F 节）T-02b 行跟踪 |
| `tag`/`key`/`id` 设备实例指纹未脱敏 | 低（无个人身份信息） | 维持现状，可选后续项进对账滚动（第三节①） |
| bootlog 未来新增模板可能引入新泄露面 | 中期漂移风险 | 第三节③登记位 + 人工对账 |
| 非 C 系统盘路径不脱敏（redact 规则一硬编码 `C:\Users\` 前缀，非 C 盘路径不命中） | 中：非 C 盘下的用户目录名会原样留在导出与日志文本里 | 现状登记，本批不改码；盘符泛化（A–Z 枚举＋golden 回归保护）按 P2 条件触发（收到非 C 盘报障时启动） |

未发现需要**立即追加** redact 规则的新泄露点：三处必查点中 ①为低敏感维持现状、②③已被现有规则全文级覆盖。

## 五、回归测试（A 加码）

`volttray/crates/app/src/diag_export.rs` 测试模块新增 `t02b_all_fields_enumerated_and_no_user_path_leak`：

1. 构造全 20 字段填充的 `DiagExport`，所有文本型字段（`devices` 事件消息、`devices_reason`、`log_tail` 三子字段、`ChannelEnumResult.tag`/`error`、`EnumSnapshot.error`、`last_enum_reason`）埋入同一假用户名路径与设备名；
2. `to_json_string_with(&device_names)` 后断言：全文不含假用户名、不含设备名、含 `<USER>`；
3. **枚举字段名**：解析输出 JSON 顶层键集合，与本表 20 字段清单（字段数以 t02b 测试 expected 数组为准）严格相等——结构体新增/删除字段时测试失败，强制同步本文档（「使审计结论不随依赖升级漂移」的实施形态）；
4. 锁定 `tag` 维持现状结论：断言输出仍含 tag 原文形态（`7&31a4c5e6&0&0000`），防止未来误加规则静默破坏排障信息。

真机导出样例复检：**未执行**（本审计基于源码实证，真机导出行为未验证，见 commit 的 Not-Verified trailer）。

## E17 增补（2026-10-24，A-R11 落地）

- **结构化脱敏注册表**：两条全文级规则升格为 `redact.rs` 的编译期常量注册表 `REDACTION_RULES`（稳定 id `R1-user-path` / `R2-device-name`＋口径＋本文档锚点），由测试 `redaction_rules_registry_is_pinned` 钉板——规则删改/重排即测试失败，强制回本表同步。
- **整值替换优先**：规则二替换执行顺序改为按名字长度降序（编号仍按收集顺序），修复互为子串设备名的半替换失真（原「MX Keys 先于 MX Keys Mini 收集 → 设备1 Mini」残留）；安全性两序等价，仅可读性变化。由 `whole_value_priority_replaces_longest_first` 钉板。
- **字段级 diff 审计**：`diag_export.rs` 新增 `e17_field_level_diff_audit_per_05_doc`——对第二节表格**每一行**各断言一次处置口径（规则一覆盖行 8 字段逐字段断言 `<USER>`＋零残留；规则二行断言整值替换为「设备1」；维持现状行 tag/key/id 原文保留；无需覆盖行 22 项数值/枚举原值不动），任一行口径漂移定位到具体字段。
- **含令牌值的错误帧 golden**：`golden_error_frame_with_token_values`——hid open 错误帧（必查点②形态）含用户名路径＋设备名两令牌，固定输入 → 固定输出逐字相等，任何规则行为漂移打破 golden。
