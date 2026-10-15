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
├── .github/workflows/        # ci.yml（fmt 自动回推 + clippy + 测试 + 构建）
│                             # build-logi.yml / nightly.yml / bootstrap-lock.yml
└── volttray/                 # 现行工程：Rust workspace（5 crates）
    ├── Cargo.toml            # workspace 定义 + release profile（版本只改这里）
    ├── .cargo/config.toml    # MSVC 静态链接 CRT（crt-static），消除 VCRUNTIME140.dll 运行时依赖
    ├── RELEASES.md           # 各版本发布说明（v0.3.6 起；最新版本可能滞后，见 §10）
    ├── docs/
    │   ├── decisions.md      # 实现决策记录 D-01 ~ D-69（最重要的单一文档）
    │   └── experience/       # 本知识库
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

## 4. 测试与验证分层

| 层 | 能在哪跑 | 验证方式 |
| --- | --- | --- |
| `protocol` | 任意平台 | 黄金样例 + proptest，协议解析 / 编码 / 映射全锁 |
| `device` / `app` | 任意平台 | 纯逻辑单测（状态机、连败裁决、映射、诊断导出降级） |
| `tray`（cfg windows） | Windows 才有完整功能；非 Windows 平台编译为**占位 stub**（`main.rs` 的 `cfg(not(windows))` 分支：打印「仅支持 Windows」提示并退出），`cargo test --workspace` 能编过但 tray 运行时零覆盖；CI 为 windows-latest，编译与全量测试都在 Windows 跑 | 托盘 / 菜单 / Raw Input 等运行时行为**必须真机验证**——发布说明「未验证项」段的常客 |

- 全量测试（v0.3.13 时点）：**202 项**，`cargo test --workspace`。
- 测试文化：每个真机事故的修复都伴随「能区分旧行为」的回归测试（例：D-64 的红态证据测试——分档模式 map 无 Default 键，正是真机日志卡点）。
- 引用测试数时从实测读取，别从汇报抄（E7）。

## 5. 发布流程（与门禁清单一一对应）

1. 版本号独立 chore 提交：只改 `volttray/Cargo.toml`（workspace 统一版本）+ `Cargo.lock`；
2. push 触发 CI：fmt 自动回推（注意**回推错位**，见下）、`clippy -D warnings`、全量测试、release 构建并上传 artifact；
3. 打 tag 指向 chore 提交；从 CI artifact 取 EXE 传 Release；
4. Release 说明：中文、「能干什么 / 修了什么 / 要注意什么」三段、未验证项单列、附 sha256 与构建 head_sha；
5. 发布前照 [03 发布质量门禁](03-release-quality-gate.md) 逐条核验。

**CI 特性必须知道**：ci.yml 的 fmt 步骤会把 `cargo fmt` 结果自动回推 main——main HEAD 经常比构建提交新一个无 CI 的格式化提交。任何「CI 全绿」取证先对齐 head_sha（E5 / 门禁 B2）。

## 6. 红线清单（先过红线，再谈功能）

1. **零鼠标电池**：对判定休眠的设备零主动查询、零探测保活；休眠恢复只走 Windows Raw Input 被动事件；新增任何 HID 交互先论证射频成本（D-33）。
2. **诊断导出永不失败**：任何子项失败降级为空 + reason，不设整体报错出口（E4 / D-69）。
3. **渲染 / 启动失败永不杀进程**：失败先落日志、用出厂默认重试，仅系统级故障才退出（D-64，B-1 硬约束）。
4. **常量单一来源**：业务常量只定义一处（反面教材：轮询档位曾两处定义后漂移，D-62 收敛）。
5. **配置容错**：未知键忽略 + 警告、越界钳制 / 吸附，绝不因配置拒绝启动；警告逐条入日志。
6. **验证分级如实**：已验证 / 部分验证 / 未验证 / 阻塞 必须如实标注，失败与过期证据不得润色成通过。
7. **依赖克制**：不引大型框架 / 非必要依赖（例：配置为手写平面 TOML 子集解析，不引 toml crate——控 EXE 体积与供应链面，D-07）。

## 7. 头号报障场景排查入口：「设备识别不到 / 托盘 ?」

新加入者第一天最可能接到的活就是用户报障。按此顺序排查（各步依据见 [01](01-engineering-lessons.md) 对应条目）：

**第一步：拿到诊断导出 + 重建物理情境。** 让用户用托盘菜单「导出诊断信息」生成 JSON——任何子项失败都会降级为空 + reason，导出本身永不失败（E4 / D-69），放心要。同时问清三件事：鼠标当时是否在休眠、用户有没有动过它、系统是否刚重启（E4 物理解读教训：空导出可能只是鼠标在睡）。

**第二步：按字段读诊断。**

| 看哪里 | 看到什么 → 指向 |
| --- | --- |
| `log_tail`（日志尾部） | `overflow when subtracting duration from instant`、io 线程停在 `io-thread-spawned` 阶段 → E1（开机早期 panic，uptime < 60s 场景；确认版本 ≥ v0.3.13） |
| `log_tail` | `io error (handle suspect)` 连败 → 死句柄（E2）；`handshake no response`（超时 / 协议类）→ 多为设备休眠（E3） |
| `log_tail` | `rescan round N: channels=n slots=0 ...` 反复出现且 slots 恒为 0 → 0 槽位 + 死缓存句柄死锁（E2；v0.3.11 已修，旧版本唯一出口是手动刷新） |
| `last_enum`（最近枚举快照） | 候选通道数 / 各通道打开失败原因 → 枚举层问题（接收器未就绪、驱动重载，v0.3.9 / D-65 修复主线） |
| `devices` 为空数组 | 不等于 bug：先回第一步确认物理情境（E4） |

**第三步：定根因、给动作。** 休眠无应答 = 物理事实不是 bug（E3），让用户动一下鼠标即可（v0.3.11 F5 起秒级恢复）；连败 + IO 错误 = 句柄死，走 E2 自愈路径；开机早期 panic = E1，升级到 ≥ v0.3.13；配置异常 = 看启动日志的逐条 warning（红线 5）。

**边界**：排查动作同样受红线约束——不要在软件侧为排查增加对休眠设备的主动探测（红线 1 / D-33）。

## 8. 项目确立的长期原则（E15，可直接复用的决策模式）

- **零代价红线模式**：为产品确立「不得以牺牲 X 为代价」的硬约束（本项目 X = 鼠标电池），所有方案先过红线再谈功能。红线要写成可执行的否定句，并配可观测性证明未被触碰。
- **面向用户的发布说明风格**：中文，讲「能干什么 / 修了什么 / 要注意什么」，不按技术模块罗列；不兼容变更必须给迁移说明（例：v0.3.12 诊断导出 JSON 由裸数组改对象，说明中写明旧读端改取 `devices` 字段）。
- **默认值 / 配置迁移**：调整默认参数时对已有用户做**兼容迁移**而非一刀切（例：D-66 轮询档位重排，旧档值吸附到最近新档 + 警告日志）；但**无法区分的存量错误值不做自动迁移**（例：D-61 颜色字节序事故——错红与真蓝同值无法区分，误判代价高于用户重选，改为发布说明中提示重选）。
- **验证分级**：已验证 / 部分验证 / 未验证 / 阻塞如实分级，任何失败、跳过、过期证据不得润色成通过（门禁 D2）。

## 9. 关键文档导航

| 文档 | 读它解决什么 |
| --- | --- |
| 根 `README.md` | 产品功能、配置键、用户视角行为 |
| `volttray/docs/decisions.md`（D-01 ~ D-69） | 「为什么代码长这样」——每个偏差 / 裁决的证据与理由，改代码前必读相关 D 条目 |
| `volttray/RELEASES.md` | 各版本修了什么、怎么修的、未验证项 |
| [本知识库 01](01-engineering-lessons.md) | 事故教训三段式 |
| [本知识库 03](03-release-quality-gate.md) | 发布前逐条核验 |

**关于 PRD 与蓝图**：`decisions.md`、`Cargo.toml` 注释中引用的「PRD」「蓝图」是早期产品文档，**未入库**（全仓检索无对应文件）；决策的权威落点是 `decisions.md` 本身，这些引用视为历史出处标注，找不到原文不影响使用本知识库。

## 10. 已知文档滞后点（以代码为准）

- 根 `README.md`「功能列表 / 快速上手」中的轮询间隔段落仍是 v0.3.9 之前的旧档位（60/300/600/900/1800 秒、默认 600 秒）。现行档位以代码为准：`crates/app/src/config.rs` 的 `POLL_TIERS_SECS = [300, 600, 1800, 3600, 7200]`（5/10/30/60/120 分钟）、`DEFAULT_POLL_SECS = 1800`（D-66）。文档与代码冲突时**永远以代码 + RELEASES.md 为准**。
- `volttray/RELEASES.md` 当前止于 v0.3.12——v0.3.13 的发布说明只存在于 GitHub Releases 页。找最新版本的说明：先看 GitHub Releases，再对 RELEASES.md。

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
2. 通读 `decisions.md` 的 D-15 ~ D-22（协议实证）与 D-33 / D-65 ~ D-69（休眠与重连主线），理解两条主战场；
3. 读 [01 工程经验教训](01-engineering-lessons.md) 全部 6 条，把 E1 / E2 / E3 对应的代码（`reconnect.rs`、`diag_export.rs`）实地看一遍；
4. 接到用户报障时先走 §7 排查入口；领任务前读 [02 协作方法论](02-collaboration-playbook.md)，交付前照 [03 门禁清单](03-release-quality-gate.md) 自查。
