# 商标与命名 go/no-go 评估（T-14）

> 实施令 T-14（P2 · 纯流程）：本页是评估**载体**，不预写结论（无材料不造事实）。
> 评估对象：项目名「LogiVoltTray」（仓库名 `Logi-VoltTray`、产品显示名
> LogiVoltTray、EXE `volttray-tray.exe`）中的「Logi」前缀——与罗技（Logitech）
> 注册商标的近似性风险。README 许可证节既有声明「罗技（Logitech）相关商标归
> 其所有者所有，本项目与罗技公司无任何关联」仅为单方声明，不构成商标合规
> 抗辩；本评估给出的是「改名 / 保留 / 延期再议」三选一的显式决策记录。
>
> 时限（F.1 时钟挂载）：**本期 Release 日 + 30 天内完成**；逾期自动关闭并
> 显式记录「接受 Logi 命名风险」。

## 一、结论（三选一，待评估后填写）

**本节必须且只能勾选一项；评估完成前为空是本页的正常状态。**

- [ ] **go**：保留现名。理由（须引用下方核查表的具体行，不得泛泛而谈）：
  ____
- [ ] **no-go**：改名。新名：____ ；执行项引用下方成本清单逐条勾销：
  ____
- [ ] **延期**：暂缓决策。延期理由与最长延展期（须给出硬日期）：
  ____

评估人：____ 　评估日期：____ 　依据材料版本（commit）：____

## 二、主流分发渠道商标政策核查表

> 核查动作＝逐渠道确认「以现名上架/发布，政策上是否放行」。三渠道政策原文
> 链接均于 2026-10-20 实测可访问（HTTP 200）。

| 渠道 | 政策原文（链接） | 核查要点（政策怎么说） | 核查结果（待填） |
| --- | --- | --- | --- |
| Microsoft Store | [Store Policies §10.1.1 Distinct and Functional](https://learn.microsoft.com/en-us/windows/apps/publish/store-policies) | §10.1.1：应用名称不得与第三方产品/服务混淆（distinct / non-confusing）；不得在名称或图标中使用无权利的第三方商标。上架前须核：现名含「Logi」是否触发与罗技产品的混淆判定。 | ____ |
| winget 社区清单 | [Windows Package Manager repository policies §2.2](https://learn.microsoft.com/en-us/windows/package-manager/package/windows-package-manager-policies) | §2.2：清单名称/徽标不得侵犯第三方权利；`InstallerUrl` 须指向 ISV 自己的发布位置（本项目＝本仓库 Releases，满足）。上架前须核：清单 PackageName 现名的第三方商标近似性。 | ____ |
| GitHub（托管渠道） | [GitHub Trademark Policy](https://docs.github.com/en/site-policy/content-removal-policies/github-trademark-policy) | 商标所有者可就仓库名/内容中的商标使用提交下架投诉；GitHub 按投诉处理，防御性免责声明（README 既有句）不必然免责。须核：罗技是否有向 GitHub 投诉的先例记录、政策对描述性/ nominative 使用的边界。 | ____ |

## 三、改名成本清单（go/no-go 决策的量化输入）

> no-go 时逐条执行；执行状态与本页结论同步更新。

- [ ] **repo 改名与重定向**：GitHub 仓库改名后旧 URL 自动重定向（含 git remote），但外部引用（经验库、CI 徽章、文档链接）须全仓同步检索替换。改名后旧 `git ls-remote` 地址失效的克隆方需手动处理。
- [ ] **EXE 文件名**：`volttray-tray.exe` 本身不含「Logi」，若无品牌诉求可不动；若产品名整体改，EXE 名与 Release 说明同步。
- [ ] **AUMID / 自启互操作**：开机自启注册表项（HKCU Run）按 EXE 路径写入；**v0.3.19 起换版自启自愈机制（U-7：启动期自动对账修复注册表指向旧路径）使改名/换路径的用户侧成本大幅下降**——旧安装用户升级新名 EXE 后自启自动迁移，无需手动关→开。此为成本下降项。
- [ ] **文档与 badge 同步**：README（中英两版，含头部互链）、docs/experience 知识库、CI 徽章、Release 说明、`.github/` 模板中的仓库名引用逐一替换并全局检索兜底。

## 四、时限与逾期后果

- **起算**：本期 Release 日（F.1 时钟挂载：本期 Release 日 + 30 天）。
- **完成判据**：上方结论节三选一勾选且依据材料齐全。
- **逾期后果（自动执行，不再另行提醒）**：对账表 T-14 行自动关闭并显式记录
  「接受 Logi 命名风险」；此后因「Logi」命名引发的任何渠道拦截、投诉或下架，
  按「已知自留风险」处置，不再回溯立项。
