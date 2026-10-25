# LogiVoltTray 项目经验知识库

> **内容同步至 v0.3.23 / 2026-10-25**（每次发布回写时同步更新本戳，见「维护约定」）。
>
> 这个文件夹沉淀本项目（2026-09 至 2026-10 真实迭代）的工程教训与协作方法论。
> 注：仓库于 2026-10-11 重建，此前素材以重建时合并入库为准。
> E16–E25 为 2026-10-17 增补，覆盖 v0.3.14 之后至 v0.3.15 发布 + T6 修复合并 + 三个设计包（WP-A/B/C）定稿期间的新经验。
> E26 为 2026-10-17 增补：版本发布后经验回写机制（用户常设规则，首个适用版本 v0.3.16）——每次发版后按此机制把链路经验回写进本文件夹。
> E27–E34 为 2026-10-19 增补，覆盖 v0.3.16–v0.3.19 四个版本链路的新经验（E26 机制首次版本回写执行）。
> E35–E37 为 2026-10-24 增补，覆盖 v0.3.23 发版链路（红蓝对抗整改落地批、nightly 双根因处置、签名链演练与启用）的新经验。
> E38 为 2026-10-25 增补：收口通道不能只有一条——完成事件失灵与收口未触达，用户视角的停滞就是事故（锚定 2026-10-24 v0.3.23 发版管线收口断链事件）。
> 目标：**新加入的 AI（或新参与者）仅凭本文件夹即可有效吸收过往经验**；通用读者也能带走可迁移的方法论。

## 这是什么

LogiVoltTray 是一个 Rust 实现的 Windows 托盘罗技设备电量监控工具（5-crate workspace，公开仓库）。在 v0.3.x 系列迭代中，团队经历了多次真机事故、质检打回与发布复盘，沉淀出 38 条带真实案例的经验（编号 **E1–E38**；原始素材包不入库，每条经验以仓内 `volttray/docs/decisions.md` 的 D 编号、commit、`volttray/RELEASES.md` 与 GitHub Release 页为溯源途径）。

每条经验的固定结构：

- **教训**：一句话可执行的结论，拒绝空泛口号；
- **真实案例**：版本号 / commit / 决策记录（D-xx）级证据；
- **可执行做法**：下次遇到同类问题具体怎么做；
- **溯源锚点**：仓库内可核对的锚点（`volttray/docs/decisions.md` 的 D 编号、`volttray/RELEASES.md`、commit、Release 页）——多数条目单列「溯源」段，部分条目把锚点直接嵌在案例与做法行内，两种形态都算数。

## 给谁用、怎么用

| 读者 | 推荐阅读路径 |
| --- | --- |
| 新加入本项目的 AI / 开发者 | 先读 [04 上手指引](04-onboarding.md) 建立全貌与红线 → 再读 [01 工程经验教训](01-engineering-lessons.md) 吸收事故教训 → 动手前对照 [03 发布质量门禁](03-release-quality-gate.md) |
| 接到用户报障的人 | 直接走 [04 §7 报障排查入口](04-onboarding.md) 的字段级决策表 |
| 准备发布的人 | 直接照 [03 发布质量门禁](03-release-quality-gate.md) 逐条核验 |
| 多智能体 / 多人团队协作 | [02 协作与流程方法论](02-collaboration-playbook.md)，全部与具体平台解耦，可直接迁移 |
| 通用读者（不碰本仓库） | [02](02-collaboration-playbook.md) 全篇，与 [03](03-release-quality-gate.md) 的清单结构与核验思路；[01](01-engineering-lessons.md) 的 Windows/Rust/CI 教训也有跨项目价值 |

## 文件导航

| 文件 | 内容 | 覆盖经验 |
| --- | --- | --- |
| [01-engineering-lessons.md](01-engineering-lessons.md) | 工程经验教训：Windows 时间算术、HID 句柄失效、HID++ 协议与休眠、诊断导出设计、CI/GitHub 实战、Rust 托盘应用杂项、Windows 交叉验证、设备身份锚定、托盘性能审查、平台移植评估、本地文件有界、渲染可信性与 CI 纪律 | E1–E6、E16–E20、E27–E33、E35、E37 |
| [02-collaboration-playbook.md](02-collaboration-playbook.md) | 协作与流程方法论：证据优于自述、红蓝对抗、质量门禁、卡壳判定、进度监控、断点交接、收口纪律、误指派处置、串行代码槽位、红蓝编排进阶、设计阶段红蓝、用户红线传导、决策发生链路、版本发布后经验回写机制、双角色独立质检、收口通道不能只有一条 | E7–E14、E21–E26、E34、E36、E38 |
| [03-release-quality-gate.md](03-release-quality-gate.md) | 发布质量门禁清单：逐条可核验的放行标准（含 B5 Windows 交叉验证门禁与 v0.3.15 / T6 真实拦截记录） | E5、E7、E9、E16、E17（清单化） |
| [04-onboarding.md](04-onboarding.md) | 新 AI 上手指引：项目结构、构建、测试、发布流程、红线清单、报障排查入口、长期原则、文档导航、已知文档滞后点、术语表 | E15 + 仓库-specific 指引 |
| [05-diag-export-field-audit.md](05-diag-export-field-audit.md) | 诊断导出字段审计（T-02b）：导出 JSON 全字段的内容来源 / 泄露面评估 / redact 规则覆盖逐字段登记，含回归测试守护 | T-02b 审计产物 |
| [06-release-reconciliation.md](06-release-reconciliation.md) | 发布对账（内部维护区）：对账机制、常设核验项（F.1）与本期对账表（F.2），自 03 F 节整体搬迁 | 内部维护（T/N/K/F-Ax 编号） |
| [07-unsafe-ledger.md](07-unsafe-ledger.md) | unsafe 台账（红蓝对抗改进项 E9 产物）：全仓 97 处 unsafe 逐块 SAFETY 注释登记、计数口径与发布对账复核规则 | 红蓝对抗整改产物 |

> 对外读者说明：文中 T-xx / N-x / K-x / F-Ax 等编号为内部维护编号（实施令条款与对账机制条款），仅服务项目内部对账，外部读者可忽略。

**AI 上手指南**：仓库根 [AGENTS.md](../../AGENTS.md)（任何 AI 接手本仓库的第一入口：启动序列、接任务判断、范围红线、收口交付）与 [docs/ai-orchestration.md](../ai-orchestration.md)（多智能体调度附录：派单五要素、巡检清单、A/B 类替代规程）。

## E 编号总索引

| 编号 | 所在文件 | 一句话 |
| --- | --- | --- |
| E1 | [01](01-engineering-lessons.md) | Windows 开机早期 `Instant` 减法下溢：算术必须 checked/saturating 保护 |
| E2 | [01](01-engineering-lessons.md) | 0 注册槽位 + 死缓存句柄死锁：重试机制必须覆盖空集路径 |
| E3 | [01](01-engineering-lessons.md) | HID++ 协议与鼠标休眠的工程现实：先分「休眠不应答」与「通道故障」 |
| E4 | [01](01-engineering-lessons.md) | 诊断导出永不失败：子项各自降级，覆盖最需要诊断的空状态 |
| E5 | [01](01-engineering-lessons.md) | CI / GitHub 实战陷阱：回推错位、API 权限分层、REST 备选通道 |
| E6 | [01](01-engineering-lessons.md) | Rust / Windows 托盘应用杂项：托盘折叠区、双接收器去重、常量单一来源等 |
| E7 | [02](02-collaboration-playbook.md) | 证据优于自述（第一原则） |
| E8 | [02](02-collaboration-playbook.md) | 红蓝对抗的有效形态 |
| E9 | [02](02-collaboration-playbook.md) | 质量门禁真的有用，不要跳过 |
| E10 | [02](02-collaboration-playbook.md) | 长任务「卡壳」判定：先排除正常长耗时 |
| E11 | [02](02-collaboration-playbook.md) | 进度监控要实质性，防「表面运行中」 |
| E12 | [02](02-collaboration-playbook.md) | 断点交接（唤醒卡壳成员）的手法与边界 |
| E13 | [02](02-collaboration-playbook.md) | 收口纪律：链路完成 ≠ 用户拿到结果 |
| E14 | [02](02-collaboration-playbook.md) | 误指派与派生任务的处理 |
| E15 | [04](04-onboarding.md) | 项目确立的长期原则：零代价红线、面向用户的发布说明、验证分级（见 04 §8） |
| E16 | [01](01-engineering-lessons.md) | Windows 专属代码合并前必须本地交叉验证，别把 CI 当第一道编译器 |
| E17 | [01](01-engineering-lessons.md) | 设备身份必须锚定物理设备，而不是某次枚举的通道快照 |
| E18 | [01](01-engineering-lessons.md) | 托盘应用性能审查：先定性每个周期「唤醒几次、查几次、画几次」 |
| E19 | [01](01-engineering-lessons.md) | 平台移植评估：先分层判定「能不能」，再算「值不值」 |
| E20 | [01](01-engineering-lessons.md) | 本地文件有界 & 绝不自动删除用户文件 |
| E21 | [02](02-collaboration-playbook.md) | 串行落实规则：代码槽位同一时刻只有一双手 |
| E22 | [02](02-collaboration-playbook.md) | 红蓝对抗编排进阶：裁决注入、断点防御、时序核对、取数口径 |
| E23 | [02](02-collaboration-playbook.md) | 设计阶段红蓝：方案级的「必修后放行」同样成立 |
| E24 | [02](02-collaboration-playbook.md) | 用户红线全链路传导：写进每个环节的验收标准 |
| E25 | [02](02-collaboration-playbook.md) | 决策怎么发生：比较报告 → 决策卡 → 用户拍板；「推迟」也是正式决策 |
| E26 | [02](02-collaboration-playbook.md) | 版本发布后经验回写机制（用户常设规则） |
| E27 | [01](01-engineering-lessons.md) | 派生阈值必须锚定调度源，墙钟派生与调度语义脱钩必出结构性矛盾 |
| E28 | [01](01-engineering-lessons.md) | 渲染可信性：预览与真实产物单一渲染源，几何与字体度量实测不估算 |
| E29 | [01](01-engineering-lessons.md) | 前提不成立的计算与告警，删除优于保留 |
| E30 | [01](01-engineering-lessons.md) | 指向产物的持久外部引用会随换版静默失效：启动期对账自愈 |
| E31 | [01](01-engineering-lessons.md) | CI 与门禁自身也是被审查对象：bot 直推受保护分支是反模式 |
| E32 | [01](01-engineering-lessons.md) | CI 红了的处置纪律：fix-forward 并给语义等价证明 |
| E33 | [01](01-engineering-lessons.md) | 稳态日志采样带计数，跃迁日志全量 |
| E34 | [02](02-collaboration-playbook.md) | 优化与迁移的语义代价、误伤面必须如实披露，并给出穿透 / 回退路径 |
| E35 | [01](01-engineering-lessons.md) | CI 的静默失败面：表达式属性笔误求空无报错，包级依赖扩面横向改变系统依赖 |
| E36 | [02](02-collaboration-playbook.md) | 双角色独立质检：修复类窗口同一份清单两角色独立过全量范围 |
| E37 | [01](01-engineering-lessons.md) | 签名链启用：先 dry_run 演练全链，工具支持与权限分层逐项实证 |
| E38 | [02](02-collaboration-playbook.md) | 收口通道不能只有一条：完成事件失灵与收口未触达，用户视角的停滞就是事故 |

## 双层受众约定

- **仓库-specific 内容**（项目结构、红线、D 编号溯源）主要集中在 01 与 04；02 的方法论主线保持平台解耦，但其中带真实版本锚点的条目（如 E9、E22、E34，案例与溯源引用 RELEASES.md / D 编号 / commit）同样含仓库-specific 内容；
- **02 为平台解耦的通用方法论**，可直接迁移到其他项目；**03 按本项目发布流程组织**，其结构与核验思路可迁移、条目内容项目特定；
- 两层之间用链接互相引用，不互相复制内容。

## 维护约定

- 新经验按同样三段式（教训 / 案例 / 做法）追加到对应文件，并给一个新的 E 编号；
- **每次版本发布后的经验回写是用户确立的常设规则**，执行流程（收集素材 → 起草 → 独立审查 → 合入 main）与开放条款见 [02 E26](02-collaboration-playbook.md#e26-版本发布后经验回写机制用户常设规则)；回写时同步更新文件头部的「内容同步至」戳；
- 工程事实以仓库真实历史（commit / Release / decisions.md）为准，与经验条目冲突时以仓库为准并在条目内标注；
- 本仓库公开，**任何写入内容不得包含**：访问令牌 / 密钥、个人 ID、内部协作平台的 agent / 任务 ID、内部链接、沙箱路径。引用内部机制时一律泛化描述。
