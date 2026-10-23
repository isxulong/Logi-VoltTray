# 新 AI 上手指引

> 给新加入本项目的 AI（或新开发者）：读完这份文件即可开始有效工作。
> 覆盖：项目结构、构建、测试、发布流程、红线清单、报障排查入口、长期原则（E15）、关键文档导航、已知文档滞后点、术语表。

## 1. 项目速览

LogiVoltTray：Windows 托盘直显罗技设备电量（键盘 / 鼠标实时电量画在托盘图标上）。Rust 实现，仅支持 Windows 10/11 x64，单文件 EXE 免安装。公开仓库，MIT 许可。

## 2. 仓库结构

```
Logi-VoltTray/
├── README.md                 # 产品说明（功能 / 快速上手 / 配置 / 构建）
├── logi_*.py, red/, tests/   # 早期 Python 参考实现与协议逻辑测试
│                             # （decisions 中多次以其为「参考实现」实证协议细节）
├── .github/workflows/        # ci.yml（windows：fmt --check + clippy + 测试 + 构建；
│                             #   ubuntu：workspace clippy/test 复核 + actionlint 提示模式）
│                             # nightly.yml（fuzz 烟测 + 失败自动开单 N-2）
│                             # bootstrap-lock.yml / audit.yml（cargo audit 周检）
│                             # release.yml（发布产物构建）
│                             # traffic-snapshot.yml（访问数据周快照：T-07 门槛
│                             # 与 T-14/T-09 时钟提醒的数据源）
├── docs/
│   └── experience/           # 本知识库（仓库根级 docs/experience/）
└── volttray/                 # 现行工程：Rust workspace（5 crates）
    ├── Cargo.toml            # workspace 定义 + release profile（版本只改这里）
    ├── .cargo/config.toml    # MSVC 静态链接 CRT（crt-static），消除 VCRUNTIME140.dll 运行时依赖
    ├── RELEASES.md           # 各版本发布说明（v0.3.6 起；最新版本可能滞后，见 §10）
    ├── docs/
    │   └── decisions.md      # 实现决策记录 D-01 ~ D-84（最重要的单一文档）
    ├── fixtures/             # 协议黄金样例与测试数据（golden/、battery_map_cases.json、
    │                         # features.json、voltage_curves.json 及生成脚本 gen_golden.py）
    ├── fuzz/                 # cargo-fuzz 模糊测试入口（fuzz_targets/frame_fuzz.rs，打协议帧解析）
    ├── assets/volttray.ico
    └── crates/
        ├── protocol/         # HID++ 纯编解码：无 I/O、不依赖系统时间，黄金样例 + proptest
        ├── transport/        # HID 通道（hidapi）、事件
        ├── device/           # 设备模型：握手、电量映射、名称、状态机
        ├── app/              # 应用核心：配置、重连 / 活性裁决、诊断导出、防抖（纯逻辑可单测）
        └── tray/             # Windows 专属：托盘、菜单、设置窗口、Raw Input、bootlog（cfg windows）
```

**分层原则（理解代码的关键）**：`protocol` 不做任何 I/O；全部设备读写收敛在**单 HID IO 线程**内串行进行；跨线程只走 channel 消息，GUI 永不直接碰设备；可裁决的逻辑（重连、活性、连败）下沉 `app` 层写成**纯逻辑 + 单测锁定**。

## 3. 构建

```powershell
git clone https://github.com/isxulong/Logi-VoltTray.git
cd Logi-VoltTray\volttray
cargo build --release --locked -p volttray-tray
# 产物：volttray\target\release\volttray-tray.exe
```

- 工具链：`rust-toolchain.toml` 锁定 **1.90.0**（D-01：tray-icon 0.26 / muda 的 MSRV 为 1.90），rustup 自动按此安装；需 MSVC 构建环境；仅 Windows 可构建 tray crate 的完整功能。
- 关键依赖速览：tray-icon 0.26 / muda（托盘与菜单）、windows crate（Win32 API）、hidapi（HID 通道），均只在 `cfg(windows)` 目标下启用。
- release profile：体积优先（`opt-level="z"` + LTO + 单 codegen unit + strip，单文件 EXE ≤ 5MB 目标）；**`panic = "unwind"` 是有意选择**（PRD 要求 GUI 边界用 `catch_unwind`，不能开 abort——这也是 E1 中「线程 panic 进程不死」的制度背景）。
- 无本地 Rust 环境时：fork 后 push 触发 CI，从云端 artifact 取 EXE。

### 本地与 CI 工具导读

- **verify.sh（本地一键自检）**：仓库根 `bash verify.sh`（`--quick` 跳过交叉编译 check）一键跑 PR 门禁三件套——`cargo fmt --check` / `clippy -D warnings` / `cargo test --locked`——外加 release 目标交叉编译 check 与 commit trailer 契约 lint（提示模式）。脚本只读、不改任何文件；覆盖范围声明见脚本头注释（windows 真机环境、协议冻结 python 测试等不在其覆盖范围，完整门禁以 CI 为准）。
- **fuzz（协议帧模糊测试）**：`volttray/fuzz/`（cargo-fuzz），目标 `frame_fuzz` 打协议帧解析，本地跑 `cargo fuzz run frame_fuzz`（需 nightly 工具链）；`.github/workflows/nightly.yml` 每晚自动跑 5 分钟真实烟测，失败自动开单。「30 晚」时钟自该 workflow 合入日起算，判据与时钟挂载见 [06 F.1](06-release-reconciliation.md)。
- **fixtures 再生成**：协议黄金样例与测试数据由 `volttray/fixtures/gen_golden.py` 生成（产出 `fixtures/golden/` 向量等），协议编解码行为改动后重跑该脚本再生成，勿手改 golden 文件。
- **audit.yml 周检结果查看**：cargo audit 供应链周检（`.github/workflows/audit.yml`，每周一跑），结果在仓库 Actions 页的 audit 工作流运行记录中查看；扫描失败或发现漏洞均自动开单（label 查重防噪）。

## 4. 测试与验证分层

| 层 | 能在哪跑 | 验证方式 |
| --- | --- | --- |
| `protocol` | 任意平台 | 黄金样例 + proptest，协议解析 / 编码 / 映射全锁 |
| `device` / `app` | 任意平台 | 纯逻辑单测（状态机、连败裁决、映射、诊断导出降级） |
| `tray`（cfg windows） | Windows 才有完整功能；非 Windows 平台编译为**占位 stub**（`main.rs` 的 `cfg(not(windows))` 分支：打印「仅支持 Windows」提示并退出），`cargo test --workspace` 能编过但 tray 运行时零覆盖；CI 为 **windows-latest（编译与全量测试）＋ ubuntu-latest（ci.yml `ubuntu` job：装 libudev-dev 后跑 workspace clippy / test 复核——transport 默认特性把 hidapi 拉入全平台构建的盲区由此暴露）** | 托盘 / 菜单 / Raw Input 等运行时行为**必须真机验证**——发布说明「未验证项」段的常客 |

- 全量测试计数（**口径 + 取数命令**形态，D3 修复：数字随时点漂移，引用前重取、不落历史定值）：
  - **源码静态口径**：`grep -rc '#\[test\]' volttray/crates --include='*.rs' | awk -F: '{s+=$2} END {print s}'` —— 2026-10-22 实测 **390**（含 `cfg(windows)` 专属与各平台条件编译项；静态计数 ≠ 任何单一平台的运行时测试数）；
  - **平台实测口径**：目标平台实跑 `cargo test --workspace --locked` 计数——Windows CI（windows-latest）为全量，Linux 侧因 `cfg(windows)` 条件编译而少于全量；引用时写明平台与取数日期；
  - 每个关键数字须**两条独立取数路径且结果一致**（跨口径换算见 [02 E22](02-collaboration-playbook.md#e22-红蓝对抗编排进阶裁决注入断点防御时序核对取数口径) 取数口径段；门禁 C2：测试数从产物 / 源码实测读取，不从汇报抄）。
- 测试文化：每个真机事故的修复都伴随「能区分旧行为」的回归测试（例：D-64 的红态证据测试——分档模式 map 无 Default 键，正是真机日志卡点）。
- 引用测试数时从实测读取，别从汇报抄（E7）。

## 5. 发布流程（与门禁清单一一对应）

1. 版本号独立 chore 提交：只改 `volttray/Cargo.toml`（workspace 统一版本）+ `Cargo.lock`；
2. push 触发 CI：`cargo fmt --check`（不一致即失败、作者本地修复）、`clippy -D warnings`、全量测试、release 构建并上传 artifact；
3. 打 tag 指向 chore 提交；从 CI artifact 取 EXE 传 Release；
4. Release 说明：中文、「能干什么 / 修了什么 / 要注意什么」三段、未验证项单列、附 sha256 与构建 head_sha；
5. 发布前照 [03 发布质量门禁](03-release-quality-gate.md) 逐条核验；
6. 发布后按 [02 E26](02-collaboration-playbook.md#e26-版本发布后经验回写机制用户常设规则) 把本版本链路的有价值经验回写进本知识库（用户常设规则，首个适用版本 v0.3.16；仅文档提交，不发版、不改版本号）。

**CI 特性必须知道**：v0.3.17 起 ci.yml 的 fmt 步骤为 `cargo fmt --check`——不一致即 CI 失败、由作者本地修复后推送，不再自动回推 main（此前的自动回推机制及其「main HEAD ≠ 构建提交」错位风险，见 01 E5 与 03 门禁 B2 的历史记录）。「CI 全绿」取证仍须对齐 head_sha（门禁 B1）。

## 6. 红线清单（先过红线，再谈功能）

1. **零鼠标电池**：对判定休眠的设备零主动查询、零探测保活；休眠恢复只走 Windows Raw Input 被动事件；新增任何 HID 交互先论证射频成本（D-33）。
2. **诊断导出永不失败**：任何子项失败降级为空 + reason，不设整体报错出口（E4 / D-69）。
3. **渲染 / 启动失败永不杀进程**：失败先落日志、用出厂默认重试，仅系统级故障才退出（D-64，B-1 硬约束）。
4. **常量单一来源**：业务常量只定义一处（反面教材：轮询档位曾两处定义后漂移，D-62 收敛）。
5. **配置容错**：未知键忽略 + 警告、越界钳制 / 吸附，绝不因配置拒绝启动；警告逐条入日志。
6. **验证分级如实**：已验证 / 部分验证 / 未验证 / 阻塞 必须如实标注，失败与过期证据不得润色成通过。
7. **依赖克制**：不引大型框架 / 非必要依赖（例：配置为手写平面 TOML 子集解析，不引 toml crate——控 EXE 体积与供应链面，D-07）。
8. **绝不自动删除用户文件**：导出 / 报告等交给用户的文件只增不删，清理权归用户；程序自留地（日志等）单文件有界或轮转、文件数量不无限繁殖（E20 / v0.3.15）。

## 7. 头号报障场景排查入口：「设备识别不到 / 托盘 ?」

新加入者第一天最可能接到的活就是用户报障。按此顺序排查（各步依据见 [01](01-engineering-lessons.md) 对应条目）：

**第一步：拿到诊断导出 + 重建物理情境。** 让用户用托盘菜单「**导出诊断信息…**」导出诊断 JSON（v0.3.17 起菜单名；旧版为「诊断面板…」。v0.3.15 起弹出「另存为」对话框、保存位置用户自选）——任何子项失败都会降级为空 + reason，导出本身永不失败（E4 / D-69），放心要。同时问清三件事：鼠标当时是否在休眠、用户有没有动过它、系统是否刚重启（E4 物理解读教训：空导出可能只是鼠标在睡）。

**第二步：按字段读诊断。**

| 看哪里 | 看到什么 → 指向 |
| --- | --- |
| `log_tail`（日志尾部） | `overflow when subtracting duration from instant`、io 线程停在 `io-thread-spawned` 阶段 → E1（开机早期 panic，uptime < 60s 场景；确认版本 ≥ v0.3.13） |
| `log_tail` | `io error (handle suspect)` 连败 → 死句柄（E2）；`handshake no response`（超时 / 协议类）→ 多为设备休眠（E3） |
| `log_tail` | `rescan round N: channels=n slots=0 ...` 反复出现且 slots 恒为 0 → 0 槽位 + 死缓存句柄死锁（E2；v0.3.11 已修，旧版本唯一出口是手动刷新） |
| `last_enum`（最近枚举快照） | 候选通道数 / 各通道打开失败原因 → 枚举层问题（接收器未就绪、驱动重载，v0.3.9 / D-65 修复主线） |
| `last_enum` | 同一台鼠标出现在多条通道（有线模式枚举出 7 个 USB 接口 MI_00~MI_06、各注册一条）→ 确诊 T6 重复条目（E17），无需再查他处 |
| `devices` 为空数组 | 不等于 bug：先回第一步确认物理情境（E4） |
| `devices` 事件流 | 「电量显示不准 / 跳变」类报障：文件夹暂无已定性根因——按 E7 纪律先取证（事件流中的连续读数、电量来源回退路径、版本号与轮询档位），如实标注「未定性」，不凭猜测给用户结论 |
| 蓝牙/有线诊断导出处理（T-08 触发式归因） | 收到 ≥1 份蓝牙或直连（实验性通道）诊断导出 → 按既有原因码归因：RELEASES.md v0.3.9 段五分类（query_timeout / io_error / error_response / no_battery_features / device_gone）＋ diag_export.rs wake miss 子分类（no_path / no_match / ambiguous），结果登记对账表 T-08 行并留痕（验收＝首单登记记录）。**30 天零样本则维持现状披露，不做降级动作**（K-4，不设兜底时钟——不做任何未验证蓝牙/有线代码通路改动） |

**第三步：定根因、给动作。** 休眠无应答 = 物理事实不是 bug（E3），让用户动一下鼠标即可（v0.3.11 F5 起秒级恢复）；连败 + IO 错误 = 句柄死，走 E2 自愈路径；开机早期 panic = E1，升级到 ≥ v0.3.13；配置异常 = 看启动日志的逐条 warning（红线 5）；同一鼠标在充电时裂出多条重复条目 = T6 已知问题（有线枚举出 7 个 USB 接口所致），主干已按 D-70/D-71 修复（PhysicalKey 物理键去重 + 晋级验明，见 [01 E17](01-engineering-lessons.md#e17-设备身份必须锚定物理设备而不是某次枚举的通道快照)）；**该修复已随 v0.3.16 发布**——对 ≤v0.3.15 的用户可直接建议升级至 ≥v0.3.16（T6 修复本体见 D-70/D-71；v0.3.16 发布说明未单列该修复，发布状态依 v0.3.15…v0.3.16 提交区间核实：修复提交 `5e265e26b0b`、`700ccf80bd0` 在区间内）；开机自启勾选了却不自启 = exe 换版致注册表指向旧路径（v0.3.19 U-7），≥v0.3.19 启动期自动对账修复，旧版本手动关→开一次即可。

**边界**：排查动作同样受红线约束——不要在软件侧为排查增加对休眠设备的主动探测（红线 1 / D-33）。

**任务类型 → 必读条目路由表（P1-4：报障雏形扩到改代码 / 发布 / 移植；不加 per-entry front-matter——F-B1-6 的双源同步点漂移论证成立）**：

| 任务类型 | 动手前必读 |
| --- | --- |
| 用户报障（设备识别不到 / 托盘 ?） | 本节第一～三步 + [01 E1–E4 对应条目](01-engineering-lessons.md) + [05 诊断导出字段审计](05-diag-export-field-audit.md)（按字段读诊断） |
| 改代码（protocol / transport / device / app / tray 任一 crate） | §2 分层原则 + §6 红线清单 + `volttray/docs/decisions.md` 相关 D 条目（§9 导航）+ [01](01-engineering-lessons.md) 对应事故教训；触及 `cfg(windows)` 代码先过门禁 B5（本地交叉验证）；protocol 行为改动重跑 `gen_golden.py`（§3） |
| 发布版本 | §5 发布流程 + [03 发布质量门禁](03-release-quality-gate.md) 逐条核验 + 发布后 [06 对账 / 经验回写](06-release-reconciliation.md)（E26：回写审查顺手抽查最老 1 条 E 条目，结果一句话记入对账区） |
| 移植 / 新平台适配 | §2 仓库结构（`cfg(windows)` 边界与 tray 占位 stub 形态）+ §4 测试与验证分层（各层能在哪跑）+ §6 红线清单 + decisions.md 平台相关条目（D-01 工具链 / D-33 Raw Input 等） |

## 8. 项目确立的长期原则（E15，可直接复用的决策模式）

- **零代价红线模式**：为产品确立「不得以牺牲 X 为代价」的硬约束（本项目 X = 鼠标电池），所有方案先过红线再谈功能。红线要写成可执行的否定句，并配可观测性证明未被触碰。
- **面向用户的发布说明风格**：中文，讲「能干什么 / 修了什么 / 要注意什么」，不按技术模块罗列；不兼容变更必须给迁移说明（例：v0.3.12 诊断导出 JSON 由裸数组改对象，说明中写明旧读端改取 `devices` 字段）。
- **默认值 / 配置迁移**：调整默认参数时对已有用户做**兼容迁移**而非一刀切（例：D-66 轮询档位重排，旧档值吸附到最近新档 + 警告日志）；但**无法区分的存量错误值不做自动迁移**（例：D-61 颜色字节序事故——错红与真蓝同值无法区分，误判代价高于用户重选，改为发布说明中提示重选）。
- **验证分级**：已验证 / 部分验证 / 未验证 / 阻塞如实分级，任何失败、跳过、过期证据不得润色成通过（门禁 D2）。

## 9. 关键文档导航

| 文档 | 读它解决什么 |
| --- | --- |
| 根 `README.md` | 产品功能、配置键、用户视角行为 |
| `volttray/docs/decisions.md`（D-01 ~ D-84） | 「为什么代码长这样」——每个偏差 / 裁决的证据与理由，改代码前必读相关 D 条目 |
| `volttray/RELEASES.md` | 各版本修了什么、怎么修的、未验证项 |
| [本知识库 01](01-engineering-lessons.md) | 事故教训三段式 |
| [本知识库 02](02-collaboration-playbook.md) | 证据优于自述、红蓝对抗、收口纪律等协作方法论（E7–E14、E21–E26、E34） |
| [本知识库 03](03-release-quality-gate.md) | 发布前逐条核验 |
| [本知识库 05](05-diag-export-field-audit.md) | 诊断导出 JSON 逐字段泄露面审计（T-02b） |
| [本知识库 06](06-release-reconciliation.md) | 发布对账机制与本期对账表（内部维护区，外部读者可忽略） |
| [`docs/naming-risk-assessment.md`](../naming-risk-assessment.md) | 商标与命名 go/no-go 评估载体（T-14，结论待维护者裁决） |
| [`CONTRIBUTING.md`](../../CONTRIBUTING.md) | 贡献指南：提交规范、PR 自检清单与行为红线 |
| [`SECURITY.md`](../../SECURITY.md) | 安全策略：报告渠道、范围声明与供应链 |
| [`verify.sh`](../../verify.sh) | 本地推送前一键三件套自检（fmt / clippy / test + 交叉 check） |

**关于 PRD 与蓝图**：`decisions.md`、`Cargo.toml` 注释中引用的「PRD」「蓝图」是早期产品文档，**未入库**（全仓检索无对应文件）；决策的权威落点是 `decisions.md` 本身，这些引用视为历史出处标注，找不到原文不影响使用本知识库。

## 10. 已知文档滞后点（以代码为准）

- 根 README 下载区滞后核销机制：找最新版本先看 GitHub Releases 页，再对 `volttray/RELEASES.md`；每次发布对账时按此复核并登记核销项（最新核销：2026-10-23 核指向 v0.3.21——该版 2026-10-23 发布，[06](06-release-reconciliation.md) 对账表已含 v0.3.21 行；上一次 2026-10-22 核指向 v0.3.20）。
- 决策范围上界表述曾滞后：D-84（版本窗口计划）2026-10-23 登记进 `decisions.md` 后，本文件与 AGENTS.md 中的决策记录范围上界（原止于 D-83）未同步回写——2026-10-23 已全仓更新为含 D-84，本条滞后即日消除，不再列为在途项。
- 轮询档位曾长期存在文档旧口径（60/300/600/900/1800 秒），现已统一为 `crates/app/src/config.rs` 的 `POLL_TIERS_SECS = [300, 600, 1800, 3600, 7200]`（5/10/30/60/120 分钟）、`DEFAULT_POLL_SECS = 1800`（D-66）。文档与代码冲突时**永远以代码 + RELEASES.md 为准**。
- 「30 秒槽位扫描」旧文档表述为「每 30 秒例行枚举」，实际是嵌在枚举循环内、外层每次迭代才查门限：空闲时一个轮询周期至多扫一次（v0.3.15 起文档口径已修正，定性见 [01 E18](01-engineering-lessons.md#e18-托盘应用性能审查先定性每个周期唤醒几次查几次画几次再谈优化)）。

## 11. 术语表

| 术语 | 含义（本项目语境） |
| --- | --- |
| HID++ | 罗技设备私有协议，分 1.0 与 2.0 两族，帧格式与错误码语义不同（D-15/D-16/D-17） |
| 长帧 / 短帧 | HID++ 2.0 特性调用用 20 字节长帧（D-15）；1.0 寄存器操作用短帧 |
| 槽位（slot） | 一个接收器可配对设备的位置，扫描范围 #1–#6 加 #255（0xFF）（D-18）；空槽位应答 `ERR_INVALID_ARGUMENT` 是正常应答 |
| 通道（channel） | 一个物理接收器对应的一条 HID 通道；同型号双接收器靠含实例段的 path 标签区分（D-19） |
| 接收器（receiver / dongle） | 罗技 USB 无线接收器，主机与设备之间的中继 |
| 句柄（handle） | 主机侧打开的 HID 设备句柄；重启 / 重插 / 驱动重载后会失效（「死句柄」），需作废重建（E2） |
| 枚举 / 重枚举 | 主机侧扫描 HID 设备并识别接收器的过程；主机 API 操作、零射频，可随时做 |
| Raw Input | Windows 原始输入事件；本项目用它被动感知鼠标活动，替代对休眠设备的主动探测（D-33） |
| QPC | Windows 高精度计数器，`Instant` 的时钟源，自开机起算——开机早期用 `Instant` 减 Duration 会下溢（E1） |
| 活性裁决 | `app` 层对「通道句柄是否还活着」的纯逻辑判定（`ChannelLiveness` / `SlotScanStreak`，E2/E3） |
| 连败 | 连续失败计数；按「轮」裁决、跨槽位去重，纯超时不计入（D-67） |
| 黄金样例 / fixtures | `protocol` crate 的已知输入输出对照数据（`volttray/fixtures/`），锁定协议编解码行为 |
| proptest | 属性测试：随机生成输入，验证协议解析不 panic、编解码往返一致 |
| 门禁 | 发布前逐条核验的放行清单（见 [03](03-release-quality-gate.md)） |
| fmt 回推 | CI 自动 `cargo fmt` 并把结果回推 main 的机制，会造成「main HEAD ≠ 构建提交」的错位（E5） |
| 未验证项 | 发布说明中如实标注未能验证的边界（门禁 D2） |

## 12. 上手第一周建议路径

1. 读本文件 + 根 README，装环境跑通构建与全量测试；
2. 通读 `decisions.md` 的 D-15 ~ D-22（协议实证）与 D-33 / D-65 ~ D-69（休眠与重连主线）、D-70 ~ D-71（设备身份与 T6 修复）、D-72 ~ D-84（v0.3.16–v0.3.19 设计包落地与发布修复主线，含 D-84 版本窗口计划），理解主战场；
3. 读 [01 工程经验教训](01-engineering-lessons.md) 全部 18 条（E1–E6、E16–E20、E27–E33），把 E1 / E2 / E3 对应的代码（`reconnect.rs`、`diag_export.rs`）实地看一遍；
4. 接到用户报障时先走 §7 排查入口；领任务前读 [02 协作方法论](02-collaboration-playbook.md)，交付前照 [03 门禁清单](03-release-quality-gate.md) 自查。
