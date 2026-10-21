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
- 仓库根 `bash verify.sh` 可一键跑上述三件套（可选交叉编译 check）。注：`verify.sh` 所属工具脚本随补丁批②–⑤合入生效，本批尚未入库。

## PR 约定

- PR 与 push main 同门禁（CI 对 `pull_request` 触发）；
- 修复类改动请带最小可复现描述；涉及未真机验证的行为，在描述中如实标注「未验证」；
- 不要提交 `target/` 目录产物。

## 行为红线

HID++ 通信、托盘图标绘制、右键菜单既有语义、电量显示、多设备路径等既有行为，除条目本身要求外请勿顺手改动。
