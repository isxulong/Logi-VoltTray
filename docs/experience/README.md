# LogiVoltTray 项目经验知识库

> 这个文件夹沉淀本项目（2026-09 至 2026-10 真实迭代）的工程教训与协作方法论。
> E16–E25 为 2026-10-17 增补，覆盖 v0.3.14 之后至 v0.3.15 发布 + T6 修复合并 + 三个设计包（WP-A/B/C）定稿期间的新经验。
> 目标：**新加入的 AI（或新参与者）仅凭本文件夹即可有效吸收过往经验**；通用读者也能带走可迁移的方法论。

## 这是什么

LogiVoltTray 是一个 Rust 实现的 Windows 托盘罗技设备电量监控工具（5-crate workspace，公开仓库）。在 v0.3.x 系列迭代中，团队经历了多次真机事故、质检打回与发布复盘，沉淀出 25 条带真实案例的经验（编号 **E1–E25**；原始素材包不入库，每条经验以仓内 `volttray/docs/decisions.md` 的 D 编号、commit、`volttray/RELEASES.md` 与 GitHub Release 页为溯源途径）。

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
| [01-engineering-lessons.md](01-engineering-lessons.md) | 工程经验教训：Windows 时间算术、HID 句柄失效、HID++ 协议与休眠、诊断导出设计、CI/GitHub 实战、Rust 托盘应用杂项、Windows 交叉验证、设备身份锚定、托盘性能审查、平台移植评估、本地文件有界 | E1–E6、E16–E20 |
| [02-collaboration-playbook.md](02-collaboration-playbook.md) | 协作与流程方法论：证据优于自述、红蓝对抗、质量门禁、卡壳判定、进度监控、断点交接、收口纪律、误指派处置、串行代码槽位、红蓝编排进阶、设计阶段红蓝、用户红线传导、决策发生链路 | E7–E14、E21–E25 |
| [03-release-quality-gate.md](03-release-quality-gate.md) | 发布质量门禁清单：逐条可核验的放行标准（含 B5 Windows 交叉验证门禁与 v0.3.15 / T6 真实拦截记录） | E5、E7、E9、E16、E17（清单化） |
| [04-onboarding.md](04-onboarding.md) | 新 AI 上手指引：项目结构、构建、测试、发布流程、红线清单、报障排查入口、长期原则、文档导航、已知文档滞后点、术语表 | E15 + 仓库-specific 指引 |

## 双层受众约定

- **仓库-specific 内容**（项目结构、红线、D 编号溯源）集中在 01 与 04；
- **02 为平台解耦的通用方法论**，可直接迁移到其他项目；**03 按本项目发布流程组织**，其结构与核验思路可迁移、条目内容项目特定；
- 两层之间用链接互相引用，不互相复制内容。

## 维护约定

- 新经验按同样三段式（教训 / 案例 / 做法）追加到对应文件，并给一个新的 E 编号；
- 工程事实以仓库真实历史（commit / Release / decisions.md）为准，与经验条目冲突时以仓库为准并在条目内标注；
- 本仓库公开，**任何写入内容不得包含**：访问令牌 / 密钥、个人 ID、内部协作平台的 agent / 任务 ID、内部链接、沙箱路径。引用内部机制时一律泛化描述。
