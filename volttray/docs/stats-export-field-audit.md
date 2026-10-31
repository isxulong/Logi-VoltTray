# 统计面导出字段审计表（05 诊断面审计表 · 姊妹篇）

> 组5 N-12② 自描述导出（5a）落地物。05 诊断面审计表归 `docs/experience/`
> 治理冻结区，本表作为统计面姊妹篇落在 `volttray/docs/`，格式与口径对齐
> 05 表；治理收官后按主 AD 安排决定是否回迁合并。
>
> **D-2 声明：诊断面 33h 日志时间线字段已在 ST-DOC1 G-34 登记，本组不重复
> 立项。** 本表只覆盖统计面三件套（stats JSON / CSV / HTML volttray-data）
> 与诊断导出 JSON 新增的 manifest 字段。

## 一、manifest 自描述块（本组新增，四处落点同源）

生成单点：`crates/app/src/export_manifest.rs`（编译期静态常量 +
`manifest_json(generated_at)` 纯函数）。四处落点：

| 落点 | 位置 | 形态 |
| --- | --- | --- |
| 统计 JSON | 顶层 `manifest` 键（`render_json_events`） | 完整 JSON |
| HTML 报告 | volttray-data 顶层 `manifest` 键（`build_report_data`） | 完整 JSON |
| CSV | BOM 之后、表头之前的 `#` 注释块（`render_csv`） | `# key=value` 摘要行 |
| 诊断导出 JSON | `DiagExport.manifest` 字段 | 完整 JSON |

字段构成（schema_version=1）：

| 字段 | 口径 |
| --- | --- |
| `schema_version` | manifest 结构版本；不兼容变更 +1（BvA-8：下游按版本判读，禁止按字段存在性硬解析） |
| `app_version` | 导出程序版本（CARGO_PKG_VERSION） |
| `generated_at` | 导出时刻 epoch 秒 |
| `fields.stats_json` | 统计 JSON 顶层字段字典 |
| `fields.csv_columns` | CSV 列字典（与 CSV_HEADER 逐字一致，钉板对账） |
| `fields.html_data` | HTML volttray-data 顶层字段字典 |
| `redaction_rules` | 生效脱敏规则 id 集（与 `redact::REDACTION_RULES` 同 registry，只写规则名） |
| `notes` | 知情声明（见下） |

**BvA-6 负面清单**：manifest 不含采集时区偏移值等可定位个人的值
（`export_manifest.rs` 有钉板测试：全文扫描 `+08:00` / `tz_offset` /
`Asia/` 等 token，命中即红）。

## 二、逐字段处置知情声明（notes 原文）

1. `run_start`/`run_stop` 秒级时间线保留（U-2C 裁决原文：精度不动）。
2. HTML 首末时刻字段保留（G-34 已登记；剥离/降精度挂 U-2 拍板，本批不落地）。
3. `display_name` 型号默认保留（U-2C）；anonymize=on 时按导出会话映射为
   设备N（C-3：会话内稳定、不承诺跨导出稳定；诊断面既有编号体系不动，
   编号不承担匿名化职责）。
4. manifest 只写规则名，不含采集时区偏移值（BvA-6）。

## 三、防漂移机制（A/B 要素叠加）

- B 要素钉板：`manifest_json` 逐字钉板 + schema 版本钉板
  （`export_manifest.rs` 测试）。
- A 要素防漂移：统计 JSON / HTML volttray-data 顶层键集合 == manifest
  字段字典集合（`attribution.rs` 两个测试）；CSV 列字典 == CSV_HEADER
  （`export_manifest.rs` 测试）；诊断导出字段集漂移在 t02b 失败
  （字段清单 += `manifest`）。
- 泄露扫描：HTML 零外链扫描同款 token 对 manifest 全文钉板；BvA-6 负面
  清单钉板（见一）。
- recheck.py：`check_csv` 跳过 `#` 注释行后字节级对账（同 PR 适配）；
  `check_json` / `check_html` 为键值对账，对新增 manifest 键天然容忍。

## 四、规则三（R3-stats-display-name，组5 5b 落地）

`redact::REDACTION_RULES` 第三条规则：统计面设备型号（`display_name`）
anonymize=on 时按导出会话收集顺序替换为「设备N」。

| 要点 | 口径 |
| --- | --- |
| 映射构造 | `attribution::display_name_session_order`（Read 事件首现顺序去重、空名跳过）→ `redact::device_session_map`（收集序 → 设备N，**两面共用单点函数**，诊断面 R2 同函数同序号） |
| 稳定性 | 会话内稳定、**不承诺跨导出稳定**（C-3/BvA-7：FNV-1a 低熵可字典反查、稳定伪名助长跨文件关联，编号不承担匿名化职责） |
| 覆盖面 | CSV `display_name` 列、统计 JSON read 事件 `display_name`、HTML volttray-data `devices` 清单与 `segments` 明细（三件套同会话同设备同名，有钉板测试） |
| 默认 | 型号默认保留（U-2C）；仅 anonymize=on 且 `experimental_export_redact_display_name=true`（默认）时映射 |
| 回滚 | `experimental_export_redact_display_name=false` → 保留真实型号，导出对话框强制提示「脱敏修复已回退」（C-2，不可跳过） |

配套闸门（同批配置键，`config.rs`）：

| 键 | 默认 | 口径 |
| --- | --- | --- |
| `experimental_export_redact_display_name` | 开 | 上表回滚键 |
| `experimental_export_tz_utc` | **关** | 导出时间统一 UTC（+00:00 渲染）；实现照做、默认关（BvA-5：覆盖面实测核验前默认值不动，翻转挂 U-2 裁决）；本地 stats.jsonl 落盘侧不动 |
| `experimental_export_privacy_notice` | 开 | 保存前提示「本文件含设备型号与作息信息」（U-2C）；关闭时强制提示「脱敏修复已回退」 |

泄露扫描（组5 验收项，均有钉板测试）：anonymize=on 三件套全文搜真实
型号命中即红（`attribution.rs`）；manifest 负面清单（BvA-6）见一。

## 五、回滚

manifest 为纯增量：删除 `export_manifest.rs` 与四处接线即回滚，无配置键
（BvA 决议）。CSV 注释块随 `render_csv` 接线一并消失；recheck.py 的
`#` 行跳过对无注释旧文件向后兼容（跳过 0 行）。5b 三项配置键回滚语义
见第四节表格（C-2 强制提示 + BvA-5 默认关）。
