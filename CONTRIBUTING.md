# 贡献指南

感谢愿意为 Logi VoltTray 出力。这是一个人维护的业余项目，流程从简，但质量门槛不打折。

## 构建

构建步骤见 README「从源码构建」一节（Rust 工具链锁定见 `volttray/rust-toolchain.toml`），本页不重复。

## 提交规范

- 提交信息用 type 前缀：`fix:`（缺陷修复）、`feat:`（新行为）、`docs:`（纯文档）、`ci:`（CI/工作流）、`refactor:`（无行为变化的重构）等。
- 一个提交做一件事，便于独立回滚；不与无关改动混在同一提交。

## PR 前自检清单

本地全绿后再发起 PR（CI 在 windows-latest 上跑同一套门禁）：

- `cargo fmt --all -- --check` 通过；
- `cargo clippy --workspace --all-targets --locked -- -D warnings` 零告警；
- `cargo test --workspace --locked` 全部通过；
- 仓库根 `bash verify.sh` 可一键跑上述三件套（可选交叉编译 check）。

## PR 约定

- PR 与 push main 同门禁（CI 对 `pull_request` 触发）；
- 修复类改动请带最小可复现描述；涉及未真机验证的行为，在描述中如实标注「未验证」；
- 不要提交 `target/` 目录产物。

## 行为红线

HID++ 通信、托盘图标绘制、右键菜单既有语义、电量显示、多设备路径等既有行为，除条目本身要求外请勿顺手改动。

## 工程约束（贡献前必读）

- **5 crate 分层**：protocol（纯编解码，不做 I/O）→ transport（hidapi）→ device → app（core/config/persist/stats/autostart）→ tray（Win32 UI）。分层与协议层纯编解码定位不可动。
- **零遥测**：不新增任何网络上报通道；「检查更新」仅打开固定 Releases 页 URL。
- **零电池红线（D-33）**：休眠设备零主动查询；任何轮询/唤醒逻辑改动须保持该红线，评审时逐条过 diff。
- **新依赖准入**：先跑 `cargo audit`，再评估 `cfg` 条件与平台影响，最后在 PR 描述中说明引入理由。
- **工具链锁定**：`rust-toolchain.toml` 钉 1.90.0，勿在贡献中升级。
- **触及 `cfg(windows)` 代码**：推送前必须做 Windows target 本地交叉验证（门禁 B5；CI ubuntu job 也有 windows-target check 步兜底）。

## 发版流程

发版 runbook、回退到 N-1 标准段与发布契约断言说明见 `volttray/RELEASES.md` 头部；发布门禁清单见 `docs/experience/03-release-quality-gate.md`。

## 待办（维护者事项，非贡献者任务）

- 仓库协作者邀请：待维护者在仓库设置中添加（本文件不列具体人选，邀请后在此登记协作分工）。
