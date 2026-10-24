# 05 · 诊断导出字段审计（T-02b）

> 定位：P1 · 审查文档 + 可回归测试。审计对象＝诊断导出 JSON（「导出诊断信息…」菜单产生的文件）的全部序列化字段，逐字段登记内容来源 / 泄露面评估 / 已覆盖的 redact 规则。三处必查意外泄露点专节见第三节。
>
> 审计基线：分支 `adversarial-impl-2026-10`，代码位置以审计当轮 HEAD 为准——`volttray/crates/app/src/diag_export.rs`（结构体定义）、`volttray/crates/app/src/redact.rs`（文本侧脱敏）、`volttray/crates/tray/src/main.rs`（export_diag 组装与 ChannelEnumResult 构造）、`volttray/crates/transport/src/hid_impl.rs`（tag/key/错误文本来源）、`volttray/crates/transport/src/events.rs`（DiagKind 事件形态）。
>
> 结论由代码实证得出，非预填：每条「已覆盖」都给出机制依据，「未覆盖」给出处置结论。审计结论的可回归性由 `diag_export.rs` 测试 `t02b_all_fields_enumerated_and_no_user_path_leak` 守护（见第五节）——结构体字段集漂移时测试失败，强制同步本文档。

## 一、脱敏机制总述（审计的前提）

导出走 `DiagExport::to_json_string_with(&device_names)`：先把**整个结构体** `serde_json::to_string_pretty` 序列化为 JSON 文本，再对**全文**执行 `redact::redact_text`（T-02a，`redact.rs`）。因此两条规则都是**字段无关的全文级**：

- 规则一：`C:\Users\<段>` 的 `<段>` → `<USER>`，JSON 转义形态（`C:\\Users\\`）与原始形态（`C:\Users\`）都识别——任何字段值里出现该形态都会被处理，**不依赖字段名单**；
- 规则二：设备名按收集顺序 → 「设备N」。设备名由 tray 侧 `export_diag` 在同一次持锁内从事件流 `DiagKind::DeviceFound` 收集（`main.rs` L3344 起）；锁中毒时收集为空数组，仅规则一生效（T-02a 已登记残余，见第四节）。

另外 `devices` 字段（事件流）在序列化前还有**字段级**帧脱敏：`observer::export_value(true)` 对 `FrameTx`/`FrameRx` 的 `hex` 按 features.json 的 `sensitive_offsets` 遮蔽（`mask_event`，P1-6）。

## 二、DiagExport 逐字段审计

结构体定义 `diag_export.rs` L105 起。19 个序列化字段（含 `skip_serializing_if` 可选字段；E22 新增 `wake_recovery_via_reenum`）：

| 字段 | 内容来源 | 泄露面评估 | redact 覆盖 |
| --- | --- | --- | --- |
| `devices` | `observer.export_value(true)`：环形缓冲 256 条 `DiagEvent` 的 JSON 数组（`main.rs` L3344） | 事件含设备名（`DeviceFound.name`）与帧 hex；hex 已按 sensitive_offsets 遮蔽；事件无路径字段 | 规则二覆盖设备名；无路径面 |
| `devices_reason`（skip） | 固定值 `"observer lock poisoned"` | 无个人信息 | 无需覆盖 |
| `log_tail.path` | `read_log_tail` 的 `path.to_string_lossy()`：bootlog 实际落点（APPDATA → EXE 目录 → TEMP 逐级兜底后的路径） | **含 `C:\Users\<用户名>` 主干**（APPDATA 前缀） | 规则一（双形态）覆盖 |
| `log_tail.lines` | bootlog 尾部最多 200 行 | 设置类日志；`persist {key} to {路径} failed` 等模板可能含 APPDATA 路径（必查点三，见第三节） | 规则一覆盖路径形态；设备名见第三节残余分析 |
| `log_tail.reason` | `format!("log read failed: {e}")` / `"log file is empty"` | `std::io::Error` 的 Display 不含路径（os error 消息） | 无需覆盖（保守起见路径形态也已由规则一兜底） |
| `last_enum`（Option，null 或快照） | `enum_snap` 内存快照克隆 | 嵌套结构见下表 | 见下表 |
| `last_enum_reason`（skip） | 固定值 `"enum snapshot lock poisoned"` | 无个人信息 | 无需覆盖 |
| `channel_map_miss` / `frame_tx` / `frame_rx` | 原子计数器 | 纯数值 | 无需覆盖 |
| `avg_tx_interval_ms`（skip） | 帧间隔均值 | 纯数值 | 无需覆盖 |
| `feature_index_tx` / `feature_index_rx` | `FrameStats`：键为 `format!("0x{f:02X}")`（HID++ 帧第 3 字节格式化，`main.rs` L243/L250） | 键是协议功能索引号，非用户数据 | 无需覆盖 |
| `arm_all_fills` / `wake_successes` / `wake_miss_no_path` / `wake_miss_zero_match` / `wake_miss_no_fingerprint` / `wake_miss_ambiguous` / `wake_recovery_via_reenum`（E22 新增：唤醒信号窗口内经整轮重枚举恢复注册计数） | 原子计数器 | 纯数值 | 无需覆盖 |
| `exported_at` | unix 秒 | 时间戳，无个人标识 | 无需覆盖 |
| `app_version` | `CARGO_PKG_VERSION` | 版本号 | 无需覆盖 |

嵌套结构：

| 结构 / 字段 | 内容来源 | 泄露面评估 | redact 覆盖 |
| --- | --- | --- | --- |
| `EnumSnapshot.at_ms` / `forced` / `candidate_channels` / `registered_slots` | 枚举时刻数值 | 无 | 无需覆盖 |
| `EnumSnapshot.error`（skip） | 枚举过程错误 `e.to_string()`（hidapi init/枚举错误原文） | 错误原文可能含设备路径 → 形态上属 `C:\Users\<段>` 即被规则一全文覆盖；hidapi Windows 错误文本一般不含用户名 | 规则一兜底覆盖 |
| `ChannelEnumResult.key` | `format!("{:04X}|{}", pid, tag)`（`hid_impl.rs` L159） | 实例指纹段（同 tag），无用户名/路径/设备名 | 不命中两规则；维持现状（同 tag 处置） |
| `ChannelEnumResult.pid` | `format!("0x{:04X}", pid)` | 硬件 PID | 无需覆盖 |
| `ChannelEnumResult.tag` | `path_tag(d.path())`：HID 实例路径 `#` 分隔第二段去 `&ColNN` 后缀（`hid_impl.rs` L164-174） | **必查点一**，见第三节 | 不命中两规则（形态无 `C:\Users`、非设备名）；处置见第三节 |
| `ChannelEnumResult.open` | 固定枚举：`"ok"` / `"failed"` / `"cached"` / `"reopened"` / `"cached-reopen-failed"`（`main.rs` L1218/L1240/L1255 三处构造点） | 无 | 无需覆盖 |
| `ChannelEnumResult.error`（skip） | hid open 失败 `e.to_string()`（classify_io 后 `IoError::Other` 保留原文，`main.rs` L1246） | **必查点二**，见第三节 | 规则一兜底覆盖（路径形态） |
| `DiagEvent` 各变体（`devices` 内） | `events.rs`：`id` = `format!("{}#{}", info.key(), slot)`（`main.rs` L467） | `id` 形态 `046D\|7&31a4c5e6&0&0000#3`——pid|tag#slot，实例指纹（同 tag 处置）；`name` 设备名（规则二）；`IoError.message` / `BatteryUnknown.detail` / `Alert.message` / `Info.message` 错误与提示原文（路径形态由规则一兜底）；帧 hex 已 mask | 规则二覆盖 `name`；规则一兜底 message 类字段 |

## 三、三处必查泄露点（实施令点名）

**① `ChannelEnumResult.tag`（BTHHID 蓝牙实例 ID 形态）**

BTHHID 路径实证（`wakechannel.rs` L153-155 测试内）：`\\?\BTHHID#{00001124-0000-1000-8000-00805f9b34fb}_VID&0002046d_PID_b01a#9&1c2d3e4f&0&0000#{4d1e55b2-...}`——`path_tag` 取 `#` 分隔第二段，即 `9&1c2d3e4f&0&0000`。USB 形态同构：`7&DEADBEEF&0&0001`。

评估：不含用户名、不含文件路径、不含设备产品名；是 Windows 生成的设备实例 ID（同设备跨导出稳定，**可作设备指纹**）。敏感度等同 VID/PID 序列号级——用于排障的核心信息（通道 key 配对、同型号去重、BTHHID 降级判断均依赖）。

处置结论：**不追加 redact 规则**。理由：泄露面为低敏感硬件指纹而非个人身份信息；若未来裁决要求模糊化，必须对 `tag` 与含 tag 原文的 `key`/`id`（`devices` 事件内）**三处同步**处理，违反「只追加、不删改既有语义」的本期边界——登记为可选后续项，进 06 文档（06-release-reconciliation.md）F 节对账滚动。

**② `ChannelEnumResult.error`（hid open 错误原文可能含路径）**

来源：`HidDeviceChannel::open` 的 `e.to_string()` 经 `classify_io`（`hid_impl.rs` L45/L73），`IoError::Other` 保留原文；`EnumSnapshot.error` 同机制。hidapi Windows 错误文本无法静态排除含路径的可能。

覆盖判定：**已覆盖**。redact 发生在序列化后的全文上（第一节），错误文本中任何 `C:\Users\<段>` 形态（无论 JSON 转义或原始）都被规则一替换——`redact.rs` 测试 `redacts_username_in_raw_paths` 即为此形态的等价断言；含设备名则被规则二覆盖。非路径错误文本（`"device not connected"` 类）不含个人信息。

**③ `log_tail.lines`（设置类日志）**

覆盖判定：**路径形态已覆盖**——bootlog 全部消息模板本轮逐一核验（`main.rs` 中 `bootlog::note` 调用共 88 处，含 `format!` 模板、多行构造与常量字符串；原稿误写「22 个 `format!` 模板 + 10 个常量字符串 = 32 处」，R1 评审更正，88 为批⑤合入时 `rg -c 'bootlog::note'` 对 main 现状实测），含路径的模板两处：`persist {key} to {} failed: {e}`（settings 写回失败，目标为 APPDATA 下配置文件）与 `default config persist to {} failed: {e2}`（默认配置落盘失败，main.rs L2410；原稿漏数此模板，R1 评审更正），均命中规则一；其余模板为状态码/计数/错误消息，不含用户名。**bootlog 当前不记录设备名**（模板核验结论）。

残余（F-A2 裁决遗留，诚实登记）：observer 锁中毒时 `device_names` 收集为空、规则二失效。该残余在 lines 侧的**当前实际暴露为零**（bootlog 模板不含设备名），但**未来防线**存在缺口：若后续新增记录设备名的 `bootlog::note` 调用，该设备名不受规则二保护（锁中毒场景下）。处置：①不硬堵（F-A2 裁决：真正兜底是本审计）；②本节作为该缺口的登记位——后续新增 bootlog 消息模板若含设备名/用户名，须回本表补行并评估是否追加规则；③第五节回归测试对「字段集漂移」的强制同步不覆盖「消息模板漂移」，后者依赖本登记与人工对账。

## 四、残余与处置结论汇总

| 残余 | 风险 | 处置 |
| --- | --- | --- |
| 锁中毒时规则二失效（T-02a 已登记，F-A2 裁决不硬堵） | events 侧：锁中毒时 `devices` 本身降级为空数组，无设备名可泄；lines 侧：当前模板无设备名，实际暴露为零 | 本审计第三节③为登记位；对账表（06 文档 F 节）T-02b 行跟踪 |
| `tag`/`key`/`id` 设备实例指纹未脱敏 | 低（无个人身份信息） | 维持现状，可选后续项进对账滚动（第三节①） |
| bootlog 未来新增模板可能引入新泄露面 | 中期漂移风险 | 第三节③登记位 + 人工对账 |

未发现需要**立即追加** redact 规则的新泄露点：三处必查点中 ①为低敏感维持现状、②③已被现有规则全文级覆盖。

## 五、回归测试（A 加码）

`volttray/crates/app/src/diag_export.rs` 测试模块新增 `t02b_all_fields_enumerated_and_no_user_path_leak`：

1. 构造全 19 字段填充的 `DiagExport`，所有文本型字段（`devices` 事件消息、`devices_reason`、`log_tail` 三子字段、`ChannelEnumResult.tag`/`error`、`EnumSnapshot.error`、`last_enum_reason`）埋入同一假用户名路径与设备名；
2. `to_json_string_with(&device_names)` 后断言：全文不含假用户名、不含设备名、含 `<USER>`；
3. **枚举字段名**：解析输出 JSON 顶层键集合，与本表 19 字段清单严格相等——结构体新增/删除字段时测试失败，强制同步本文档（「使审计结论不随依赖升级漂移」的实施形态）；
4. 锁定 `tag` 维持现状结论：断言输出仍含 tag 原文形态（`7&31a4c5e6&0&0000`），防止未来误加规则静默破坏排障信息。

真机导出样例复检：**未执行**（本审计基于源码实证，真机导出行为未验证，见 commit 的 Not-Verified trailer）。
