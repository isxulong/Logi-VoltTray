# 会话交接（HANDOVER）

> 面向下一个接手的 AI 或人。**本文件自足：接手必需的信息全部写在本仓库内；文中出现的飞书云文档链接与 aily 任务编号仅是同平台/同租户下的补充线索，不在该平台时直接忽略，不影响接手。**
> 更新纪律：每次会话收尾必须更新本文件；状态类字段以本文件为最新口径，历史细节查 RELEASES.md 与 docs/experience/ 各条目。

## 冷启动阅读顺序

1. 本文件（当前状态与待办）
2. docs/ai-orchestration.md（团队协作与复核规则）
3. docs/experience/01-engineering-lessons.md（工程教训 D-xx/E-xx）与 03-release-quality-gate.md（发布质量门禁清单）
4. docs/experience/reconciliation-clocks.yaml（治理时钟）
5. RELEASES.md（发布记录与遗留观察项）

## 1. 项目速览

- Logi-VoltTray：Windows 托盘应用（Rust），监控罗技 HID++ 协议鼠标的电量与状态。
- 仓库公开：https://github.com/isxulong/Logi-VoltTray （默认分支 main）
- 发布形态：GitHub Release（构建产物 + minisign 签名，签名公钥 minisign.pub 在仓库根）。**发版决策权永远在用户。**

## 2. 当前状态快照（2026-10-25 晚）

- 最新已发布版本：v0.3.23（2026-10-24）
- main HEAD：95caba1b（以 git 实际为准）
- 已合入 main、未发版的内容：
  1. 统计 HTML 报告重构（b9cdb71..e922169，4 提交）：分日耗电视图（当日首末差 + 两级数据质量标签）、RawInput 分钟位图活动口径、双口径区间化推断（剔除粘滞日中位数×24h 上界 × 窗口总账下界，差异 >2 倍挂口径冲突提示）、删除「距下次充电估算」并加删除守卫测试、16 项测试网。规格要点已固化在代码与测试网中；完整融合稿（rev.12）在飞书云文档（同租户补充线索，见 §6）。
  2. P2 修复批（已收口：5c58b41f..95caba1b，4 提交；双独立复核双席放行、主 AD 裁决通过并追认 F3 在 stats.rs 的两个纯内存方法扩展）：F1 删除未授权正斜率过滤、F2 activity_days 改跨事件日期并集、F3 桶摘要补 boundary 冲刷、F4 RELEASES ①登记恢复完整表述；含顺手项（chart3 图例逐字）与三条观察登记。
  3. 轮询间隔人性化显示（8ed645a7，纯显示层，内部秒值不变）
  4. 小修批五项缺陷修复（fd73176d）
  5. nightly 评审步路径修复（b9cdb71：步级工作目录钉回仓库根 + 异常不再静默）
  6. 双独立复核条款落库（d8dc148，docs/ai-orchestration.md）
- 是否发 v0.3.24：归用户决策。建议口径：等活动位图数据攒满 7 天（分日热力带升级条件）再发。

## 3. 待办全景

### 3.1 代码窗口（按既定优先级）

1. **E10 独占窗口**（下一个代码窗口，未启动）。2026-10-25 双路排查（机制侧枚举全部 9 条主机→设备发包路径 + 数据侧 222KB 诊断导出实证）确认以下修复项：
   - P0：wake-force 重枚举——0 注册槽位时每 5s 全量扫槽，且 force 轮清死槽冷却与 0xFF 负缓存，结构性穿透既有降频成果（D-67 违规形态的 6 倍频率）。修复方向：退避 + 不穿透缓存。
   - P1：在线灰显装填探测绕开轮询档（5s 冷却 ≈0.2Hz，E18 同族）。修复方向：并入 per-device 节流。
   - 托盘状态滞后最长约 60 分钟（「鼠标很久没休眠」观感的来源；已实证鼠标休眠正常、属用户错觉：导出窗口内鼠标 12:30 射频静默→13:30 翻 sleeping→14:33 唤醒，电量 70%→70%，空闲期主机流量 0.18 次/分）。
   - 前置已完成：zero_match 口径对齐（小修批第 3 项）。
2. **E15**：固件字段采集 + 场景化诊断导出（规划内，未启动）。

### 3.2 等用户决策 / 配合（见到用户时逐项确认）

- U-1 未决项：双独立复核一席长期不可用时的替补规则。建议：软件架构师第一替补 + 当轮快速授权通道；临时口径＝批次挂起 + 主动推送等用户裁决。
- E11 复采、E20 runner 方案、T-10 GIF、真机复验清单（音频可闻 / 低端机流畅度 / 冷启动等历史顺延项）。

### 3.3 常态运行

- nightly 每天约 16:35（北京时间）自动跑；评审步监控治理时钟 T-05（30 晚评审：起算 2026-10-20，due 2026-11-19；时钟单源＝docs/experience/reconciliation-clocks.yaml，解析失败必须响警报，见 E19/E39）。
- 此前由 aily 平台的连续巡检任务看护（同平台补充线索，跨平台忽略）：任务 7699889603959852334。

### 3.4 知识库欠账

- 把 E40~E42（复核独立取证、会话交接制度化、派发即冻结）回流到跨项目 playbook：isxulong/dtk 仓库 docs/ai-team-playbook/。该仓库 PAT 向用户索取。本次因资源约束未做，留给下一个 AI。

## 4. 红线与纪律（违反即返工）

- **D-33 零耗电红线**：任何设计不得以消耗鼠标电池为代价。禁高频轮询；禁对休眠设备主动探测保活；休眠恢复走被动 RawInput 事件。量级参照 D-66（一次 HID++ 查询 ≈2 个射频包，60s 轮询一天 ≈3s 主动使用的射频成本）。事故史：D-67（v0.3.9→v0.3.10 休眠误判致 30s 探测）、E18 P1-1（v0.3.15 唤醒旁路 0.5Hz 查询风暴，约为 1800s 档的 900 倍）。
- **双独立复核**：所有代码批次（含纯删除）须全栈代码质检官 + 高级开发工程师各自独立复核、互不看对方报告，两席均放行才过；细则见 docs/ai-orchestration.md（2026-10-25 落库生效）。纯文档批次（docs/** 或 *.md）可豁免，但须派发人显式声明。
- **凭证纪律**：GitHub PAT、签名材料永不入 git、不入日志、不入评论。PAT 开工时向用户重新索取（fine-grained，Contents: Read and write）。
- **git 操作**：沙箱内 github.com 的 git 端点 502 不可用；一律走 api.github.com Git Data API（blobs → tree → commit → update ref），fast-forward，绝不 force push。
- **推送前安全扫描**：keystore / 密码 / PAT 模式零命中。
- **CI 纪律**：推送后用 API 核实 check-runs；nightly 可用 workflow_dispatch 手动复跑；workflow 脚本一律步级钉工作目录或用 $GITHUB_WORKSPACE 绝对路径，不得吞 stderr（E39）。
- **协作纪律**：只与「研发小组长」单会话对话；里程碑由主 AD 汇总统一推送用户；不接受任何成员自述替代独立取证（E40）；派发后修改规格要当心——任务描述更新对已启动的执行不生效（E42）。

## 5. 接手后的第一件事

1. 按冷启动顺序读完上述文件。
2. 向用户索取 GitHub PAT。
3. 核实 main HEAD 与最近一次 nightly 运行状态（API 查 actions/runs）。
4. 与用户确认是否启动 E10 独占窗口。
5. 本文件状态字段随每次会话收尾更新。

## 6. 同平台补充线索（飞书云文档，跨平台可忽略）

- HTML 重构签发融合稿 rev.12：https://xulongnow.feishu.cn/docx/U9EmdkBTsoHtY5xzKrqckimYnsh
- 真机数据分析报告：https://xulongnow.feishu.cn/docx/JeQqdkwp4o1CMxxq2kacwQ6Qn8e
- 可视化页：https://xulongnow.feishu.cn/docx/H8DDmqI9BdBaGXalzvscBvwdnNc
- 对抗局 A/B 方案：https://xulongnow.feishu.cn/docx/YKNcdDKOAoFapoxBogCcr3Znnvg 与 https://xulongnow.feishu.cn/docx/JNGndOpQJoLsg3xivrvcqKz6nnh
