# 实现决策记录（蓝图缺漏 / 偏差的就地裁决）

依据任务约定：蓝图有缺漏时按 PRD 与红蓝评审裁决就地决策并记录；不推翻蓝图主体。

| 编号 | 决策 | 依据 |
| --- | --- | --- |
| D-01 | 工具链锁定 `1.90.0`（rust-toolchain.toml + CI dtolnay/rust-toolchain@1.90.0） | 蓝图示例为 1.85.0，但 tray-icon 0.26 / muda 的 MSRV 为 1.90，蓝图版本无法编译；取满足依赖的最小稳定版 |
| D-02 | `QueryCommand::encode_for(device_index, sw_id, &FeatureTable)` | 蓝图字面签名缺 FeatureTable 参数，但命令编码必须查特性索引表；参数补齐而非改变语义 |
| D-03 | HID Power Descriptor 回退为尽力而为（尝试 report id 0x04–0x07，取首个 percent≤100 者） | 蓝图未给 report id；PRD 只要求"回退到 HID Power"。真机未验证，导出诊断可见实际命中 |
| D-04 | hidapi 读回 6/19 字节时按长度补 report id | hidapi 跨平台对首字节处理不一致；归一化在 transport 内完成 |
| D-05 | HID++ 接口筛选 usage page {0xFF43, 0xFF00} | 罗技 HID++ 常用页；蓝图未列枚举 |
| D-06 | 电量参数映射（unified: p0≤100 & p2&0x03==1 充电；status: p1&0x80；voltage: u16BE + p2&1；1.0: p1&0x40）真机未验证 | 蓝图/PRD 未给逐位映射；按社区文档取值，单测只锁映射函数本身 |
| D-07 | 配置解析为手写平面 TOML 子集（7 键，P1-8 schema 固定） | 评审 P1-8 已定死 schema；不引 toml 依赖以控 EXE 体积与供应链面 |
| D-08 | 默认展示设备 = 未充电且未置灰中电量最低者；否则取未置灰第一台；再否则 None | PRD 未定义多设备默认展示规则 |
| D-09 | AUMID 快捷方式用 IShellLinkW + IPropertyStore（手写 PROPVARIANT VT_LPWSTR，PKEY fmtid 9F4C2855-…-E1D42DE1D5F3 pid 5） | P0-5；windows crate 未提供现成 PKEY 常量，按 SDK 头文件值手工构造 |
| D-10 | 诊断面板以「导出脱敏 JSON 到配置目录 + MessageBox 告知路径」实现，未做裸 Win32 对话框 | 蓝图对话框为建议形态；导出+查看功能等价且 UI 风险最小（P1-6 脱敏导出不打折） |
| D-11 | 接收器多设备枚举延后：本轮仅轮询 device_index 0xFF（接收器直连设备） | 蓝图未覆盖多设备枚举时序；Device 模型已保留 device_index 字段供后续扩展 |
| D-12 | GetFeatureIndex 经 IRoot index 0 fn 0 编码（规范自举） | 若经 IFeatureSet 自身索引编码则形成自举环；HID++ 2.0 规范定义为 IRoot fn0 |
| D-13 | GUI 线程的通知开关（notifications_enabled / silent）取启动时配置快照，热重载仅作用于 IO 线程行为 | 简化跨线程配置分发；重启后生效，不影响 IO 侧轮询/阈值热更新 |
| D-14 | 图标文本绘制用 TextOutW（非 DrawTextW） | 单行居中已由位图布局保证；TextOutW 参数面更小 |
