# 实现决策记录（蓝图缺漏 / 偏差的就地裁决）

依据任务约定：蓝图有缺漏时按 PRD 与红蓝评审裁决就地决策并记录；不推翻蓝图主体。

| 编号 | 决策 | 依据 |
| --- | --- | --- |
| D-01 | 工具链锁定 `1.90.0`（rust-toolchain.toml + CI dtolnay/rust-toolchain@1.90.0） | 蓝图示例为 1.85.0，但 tray-icon 0.26 / muda 的 MSRV 为 1.90，蓝图版本无法编译；取满足依赖的最小稳定版 |
| D-02 | `QueryCommand::encode_for(device_index, sw_id, &FeatureTable)` | 蓝图字面签名缺 FeatureTable 参数，但命令编码必须查特性索引表；参数补齐而非改变语义 |
| D-03 | HID Power Descriptor 回退为尽力而为（尝试 report id 0x04–0x07，取首个 percent≤100 者） | 蓝图未给 report id；PRD 只要求"回退到 HID Power"。真机未验证，导出诊断可见实际命中 |
| D-04 | hidapi 读回 6/19 字节时按长度补 report id | hidapi 跨平台对首字节处理不一致；归一化在 transport 内完成 |
| D-05 | HID++ 接口筛选 usage page {0xFF43, 0xFF00} | 罗技 HID++ 常用页；蓝图未列枚举 |
| D-06 | 电量参数映射（unified: p0≤100 & p2&0x03==1 充电；status: p1&0x80；voltage: u16BE + p2&1；1.0: p1&0x40）真机未验证 | 蓝图/PRD 未给逐位映射；按社区文档取值，单测只锁映射函数本身。**v0.2.0 注：已被 D-22 实证值推翻作废** |
| D-07 | 配置解析为手写平面 TOML 子集（7 键，P1-8 schema 固定） | 评审 P1-8 已定死 schema；不引 toml 依赖以控 EXE 体积与供应链面 |
| D-08 | 默认展示设备 = 未充电且未置灰中电量最低者；否则取未置灰第一台；再否则 None | PRD 未定义多设备默认展示规则 |
| D-09 | AUMID 快捷方式用 IShellLinkW + IPropertyStore（手写 PROPVARIANT VT_LPWSTR，PKEY fmtid 9F4C2855-…-E1D42DE1D5F3 pid 5） | P0-5；windows crate 未提供现成 PKEY 常量，按 SDK 头文件值手工构造 |
| D-10 | 诊断面板以「导出脱敏 JSON 到配置目录 + MessageBox 告知路径」实现，未做裸 Win32 对话框 | 蓝图对话框为建议形态；导出+查看功能等价且 UI 风险最小（P1-6 脱敏导出不打折） |
| D-11 | 接收器多设备枚举延后：本轮仅轮询 device_index 0xFF（接收器直连设备） | 蓝图未覆盖多设备枚举时序；Device 模型已保留 device_index 字段供后续扩展。**v0.2.0 注：延后决定已被 D-18 推翻，槽位扫描已落地** |
| D-12 | GetFeatureIndex 经 IRoot index 0 fn 0 编码（规范自举） | 若经 IFeatureSet 自身索引编码则形成自举环；HID++ 2.0 规范定义为 IRoot fn0 |
| D-13 | GUI 线程的通知开关（notifications_enabled / silent）取启动时配置快照，热重载仅作用于 IO 线程行为 | 简化跨线程配置分发；重启后生效，不影响 IO 侧轮询/阈值热更新 |
| D-14 | 图标文本绘制用 TextOutW（非 DrawTextW） | 单行居中已由位图布局保证；TextOutW 参数面更小 |
| D-15 | HID++ 2.0 全部特性调用使用 20 字节长帧（含 UNIFIED_BATTERY 查询走 fn1 getStatus，非 fn0） | 红蓝对抗实证：参考实现 logi_hidpp.py:265-269 全部长帧 + fn1；v0.1.0 短帧/fn0 是「？」根因之二 |
| D-16 | 1.0 寄存器读取按 `0x8100\|reg` 子地址编码（byte2=0x81，byte3=寄存器号，无 sw_id；寄存器号复用 sw_id 字段做应答回显配对） | 参考实现实证；v0.1.0 把寄存器号放 sw_id 字段发送是「？」根因之三 |
| D-17 | 错误帧按协议族分字段解码：2.0 特性回显在 byte3、1.0 地址回显在 byte4、错误码一律 byte5；1.0 code 0x08（UNKNOWN_DEVICE）判 NoResponse，其余 1.0 错误判 Hidpp1，2.0 ping 错误判 NoResponse | 红队差异 F5：同码跨族语义不同（0x08 在 1.0=设备休眠、2.0=Busy）；错误族误判是「？」根因之五 |
| D-18 | 接收器多设备枚举落地：每配对信道按槽位 1..=6 再 0xFF 扫描，**拿到电量读数才注册设备**（推翻 D-11 的延后决定；D-11 的 device_index 字段设计保留沿用） | 红队判定 v0.1.0 固定 0xFF 是「？」的决定性根因（真机读出电量的唯一差异项）；QA P1-3 同源并入本轮 |
| D-19 | 信道配对：usage_page {0xFF00,0xFF43}、usage {1,2}，按 (pid, 去重 path 标签) 分组短/长接口；短句柄缺失回退长路径；每请求前双句柄 drain 80 帧；READ_TIMEOUT=400ms | 参考实现实证（短/长接口收集配对是「？」根因之四） |
| D-20 | 策略链对 Timeout 也降级尝试下一电量 feature（I/O 错误不降级） | 参考实现 refresh() 链式兜底实证；v0.1.0 超时即中止是「？」根因之六 |
| D-21 | 电压曲线新增 `li-ion-logi`（真机 13 锚点，4186mV→100%…3500mV→0%），电压设备默认曲线改用之；插值由截断改四舍五入（半进位），4019mV→84% | 参考实现 VOLTAGE_TABLE + round() 实证（黄金用例 ref_voltage_fn0_4019mv） |
| D-22 | 电量参数映射按实证修正：unified/status state∈{1,2}=充电、{1,2,3}=已插电；voltage flags bit7=充电；1.0 0x0D level>100 按 255 折算、0x90+0% 视满电；1.0 0x07 走 ROUGH_LEVEL {7:90,5:50,3:20,1:5}；一切出口 clamp_pct | 推翻 D-06 的社区文档猜测值（D-06 记录在案作废）；红队差异 F7/F8 |
| D-23 | 「？」改为可定位：BatteryUnknown 诊断事件带 8 类原因码（no_battery_features / query_timeout / error_response / malformed_response / mapping_failure / curve_missing / io_error / device_gone），随脱敏导出可见 | 修复要求 B：真机问号必须可归因，而非仅显示 ? |
| D-24 | 可读性默认组合 = 深色圆角底（#1F1F1F）+ auto 字色（深底→白字）；新增 `icon_background`（transparent/light/dark，默认 dark）与 `icon_font_color`（auto 或 #RRGGBB，默认 auto）两键；Gray（未知/离线）恒灰字，Fixed 字色不覆盖 Gray | 真机反馈「无背景黑字不清晰」：单黑字无法同时满足深浅任务栏；底色+字色组合由配置保证两端可读，透明底保留给自选用户 |
| D-25 | 黄金样例采纳边界：蓝队 B2 原稿短帧与其自身 A4 及参考实现矛盾→按参考实现改长帧；B4（接收器名 0x2B5）未实现不采；clean_name 空串按 P1-5 回退「未知设备」、0x00 后填充按读名长度截断（两处偏差记录于 tests/battery_map.rs 头注） | 采纳边界=协议与测试实证；样本内部矛盾以参考实现源码为准 |
