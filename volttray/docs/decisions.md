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

| D-26 | 设备名「?」根因 = 槽位名分隔符 U+00B7「·」不在消毒白名单被替换为「?」；修复：白名单加「·」+ 回退名改「{base} · 槽位 {slot}」 | 真机菜单截图「USB Receiver ?1」实证；sanitize 单测 `middle_dot_separator_kept` 锁定 |
| D-27 | 真实设备名来源链：HID++ 2.0 DEVICE_NAME(0x0005) fn0 总长 + fn1 分片（滤 0x00、分片不满槽即结束），回退接收器配对名寄存器 0x2B5（Unifying p0=0x40+dev-1 / Bolt p0=0x60+dev 两变体）；全链失败用 D-26 回退名，不出裸「?」 | 蓝队设备名实证条目 + 参考实现 read_name/receiver_device_name；name.rs 6 组桩回放测试 |
| D-28 | 设置入口 = 裸 Win32 单页非模态窗口（外观/刷新/低电提醒三组 + 每组恢复默认 + 全局恢复全部），菜单「设置…」打开；单实例（重复打开只前置）、即时生效（无确定/取消）、关闭即销毁 | 产品方案 #7695737931205495999 裁决（窗口形态）；复用诊断面板裸 Win32 技术路径 |
| D-29 | GUI ↔ TOML 双向一致：设置窗口写共享 `Arc<RwLock<Config>>` + 整份 `to_toml` 落盘；菜单开关写 Arc 字段 + 单键 `persist_kv`（键缺失自动追加）；外部 TOML 改动走既有 mtime 热重载 | 产品方案「谁改都生效」；修正 v0.2.0 仅 TOML 单键改写、core 同步滞后的缺口 |
| D-30 | 低电闪烁 1Hz = 500ms WM_TIMER 翻相位，常态 ↔ 提亮约 40% 交替；触发/解除由单阈值 LowBatteryTracker（连续 2 次读数、解除阈值+5 迟滞、充电即复位）；点击托盘本次停闪、低电解除后重新武装；菜单与设置窗口均可关 | 产品方案 M11；pump 状态机实现，IO 侧只产 Alert |
| D-31 | M12 单阈值 `low_threshold`（5..=50，默认 20，滑块步进 5）取代 v0.2.0 双阈值：红档 = 用户阈值、黄档固定 50；旧键 `low_threshold_warn` 迁移合并、`low_threshold_critical` 忽略；`icon_background = "transparent"` 旧值迁移为 `icon_shape = "transparent"` | 产品方案 M12 + 配置 schema v0.3.0（11 键）；容错回退与钳制保留 |
| D-32 | 双接收器 P1-1 本轮不动：channels 已按 (pid, path 标签) 分组，多接收器展示聚合改动面超出「顺手修」范围，记录留待下轮 | 任务三约束：不为它扩大本轮范围 |
| D-33 | **休眠零耗电被动方案（v0.3.1）**：连续 2 次查询超时（`SLEEP_AFTER_TIMEOUTS=2`）判疑似休眠，迁入 `Sleeping` 态并**暂停一切主动轮询**——休眠期 `poll_decision` 一律 Skip，绝不向休眠设备发 HID++ 查询（用户硬约束：每次查询都是一次鼠标射频交互，主动查询/重试/保活才是真费电）。唤醒靠被动监听：Windows Raw Input（RIDEV_INPUTSINK + message-only 窗口）收 `WM_INPUT`，`GetRawInputDeviceInfoW` 设备名含 `VID_046D` 才视为罗技鼠标活动 → 发 `WakeSignal` → 仅对 Sleeping 设备武装唤醒探测（`WAKE_PROBE_COOLDOWN=5s`，每信号至多一次；GUI 侧 2s 合并防风暴）。休眠展示 = 灰色图标+最后已知电量数字（不打成「--/?」）+ tooltip「休眠中 · 最后已知电量」，与「读取失败/未知」分开建模；设置窗口轮询间隔下加「更短的间隔会增加鼠标耗电」提示 | 用户硬约束（20:24 设计收紧）：不能接受费鼠标电池，禁止提高轮询频率/短退避主动重试/主动探测保活；「接收器有该设备输入流量」在 Windows 层拿不到（配对鼠标呈现为系统独占 HID 鼠标，输入接口被系统占用读不到原始报告），故取系统级鼠标活动事件（Raw Input）作唤醒信号——即任务允许的「仅在有系统输入事件时才恢复轮询」替代 |
| D-34 | **注册状态滞留修复（v0.3.1）**：`record_success` 出口一律经 `mark_online(state)` 统一迁回 Online（幂等），消除 v0.3.0 upsert 保留旧 state 导致的 Disconnected/Connected 滞留（3 连败只改 state 不移台账，快照永远 disconnected → 灰显 → 默认展示排除 → 图标回退「?」=「未读取」症状）；`record_failure(gone)` 迁 Disconnected 前清零超时计/唤醒武装/冷却 | v0.3.1 根因判定③：代码证据见 core.rs v0.3.0 upsert 注释与 state.rs `mark_online` 迁移表（全态覆盖测试 `mark_online_covers_all_states`） |


| D-35 | **P1-1 非透明形状常态图标丢失背景修复（v0.3.2）**：`icon.rs IconCache::fill()` 常态分支 `(shape == IconShape::Transparent).then_some(bg_rgb)` 条件写反改回 `!=`——默认 rounded 得到 bg=None（托盘只剩前景数字无深色圆角底）、transparent 反而有底；settings.rs 预览的 `!=` 写法为同源正确参照。回归测试 `fill_normal_state_non_transparent_shape_keeps_background` 锁 fill 级行为：rounded/square 常态背景像素色精确等于 bg_rgb 且 alpha>0，透明形状同位置 alpha=0 作对照 | 全栈代码质检 P1（阻塞放行）；两处同源一正一反可互证 |
| D-36 | **P2-1 配置钳制警告不再静默丢弃（v0.3.2）**：main.rs 热重载处（watcher.poll）与启动加载处（Config::load）均改用既有 `DiagKind::ConfigReloaded { warnings }` 推送 config.rs 的越界钳制/迁移警告（ConfigWarning Display 转字符串）；热重载处按 warnings 是否为空分事件级别——空保持原 Info「config reloaded」、非空走 ConfigReloaded（警告级）；启动处仅在有警告时推送 | 全栈代码质检 P2 建议同批带走；observer/config 两侧该 DiagKind 与测试本已就位，只是调用点丢弃了返回值 |
| D-37 | **P2-3 设置镜像补齐 + reset-all 与注册表对齐（v0.3.2）**：pump cfg_changed 镜像补 `autostart`、`notification_silent` 两键；配置侧 autostart 变化（TOML 热重载 / 设置窗口）在锁外同步注册表与菜单勾选态，写失败时共享配置回写真实勾选态并落盘（与菜单开关失败处理同口径，避免持续分叉每 tick 重试）；新增 `autostart::sync(on)` 幂等入口（true 写当前 exe / false 删 Run 值），settings reset-all 调 `autostart::sync(Config::default().autostart)` 使注册表随 TOML 一起回默认，注册表失败不阻断其余重置 | 全栈代码质检 P2 建议同批带走；镜像原缺两键导致设置窗口/TOML 改 autostart 后菜单勾选态与注册表均不跟进 |
| D-38 | **本轮不做、放行后跟进**：P2-2 字色/背景各 8 色预设（当前仅自定义 HEX）；P3-1 右键菜单打开不停闪；P3-2 WM_ACTIVATE 漏 WA_CLICKACTIVE | 质检明确「放行后跟进」，不为它们扩大 v0.3.2 范围 |


| D-39 | **命名统一 LogiVoltTray（v0.3.3 #1）**：用户可见处全部改连写——托盘 tooltip、设置窗口标题、通知/Alert 标题、开始菜单快捷方式描述（aumid.rs）、诊断导出标题与文件名前缀 `LogiVoltTray-diag-export-*.json`、错误文案前缀（原「Logi VoltTray诊断导出失败」）、EXE 版本资源 ProductName/FileDescription（build.rs winres）、各 crate description；代码内 crate 名 `volttray-*` 不动 | 用户反馈 #1：「LogiVoltTray」连写更明确 |
| D-40 | **诊断导出修复（v0.3.3 #2，P1）**：根因=导出目标目录不存在时未创建（os error 3）。抽纯函数 `write_diag_export(dir, filename, json)`：先 `create_dir_all`（覆盖嵌套/多层缺失）再写文件；路径/权限错误经 `friendly_io_error` 转可读中文（区分目录不存在/无权限/其他，并附原始错误）。测试：多层嵌套目录不存在时导出成功、被同名文件阻塞时返回可读错误、文件名前缀断言 | 真机缺陷：右键「诊断面板/导出」在干净机器报 os error 3 |
| D-41 | **颜色入口改为取色面板（v0.3.3 #3）**：背景色/字体颜色/分档色点全部提供「选色…」按钮，呼出系统 `ChooseColorW` 对话框（CC_FULLOPEN 展开完整色轮+自定义色区，CC_RGBINIT 携当前色）；HEX 手输框保留且与取色双向同步（取色后回写 HEX 框，手输仍即时生效）；模态期间不持有配置借用防 WM_ACTIVATE 重入。D-38 的 P2-2（8 色预设）被本方案取代关闭 | 用户反馈 #3：手查颜色代码太原始，要求「直接选」；系统取色对话框满足验收（任务允许形态自选） |
| D-42 | **分档字体颜色（v0.3.3 #4）**：新键 `icon_font_tiers_enabled`（bool，默认 false）/`icon_font_tier_thresholds`（u8 数组，默认 [20,50]）/`icon_font_tier_colors`（hex 数组，默认红黄绿三色）；档位映射 `font_tier_for_percent` = 满足 percent > t 的阈值个数（20/50 → 0-20 档0、21-50 档1、51-100 档2，即「该数字及以下归上一档」）；N 档=N-1 阈值框(ES_NUMBER, 1–99 严格递增，非法即时提示不落地)+N 色点(SS_BITMAP+SS_NOTIFY 圆点，点击走 D-41 取色)；启用分档隐藏单一字体颜色入口，关闭恢复；`normalize_font_tiers` 统一钳制/修补（档数 2..=5、越界钳 1..=99、非递增逐步+1 修复、颜色截断/从 TIER_PAD_PALETTE 补齐）；flat TOML 数组解析（Value::List），热重载生效；Gray（未知/离线）不参与分档覆盖 | 用户反馈 #4 明确交互描述；沿用 D-07 不引 toml 依赖的 flat 键值路线 |
| D-43 | **电量文本居中与清晰度（v0.3.3 #5）**：两遍渲染——首遍按估算位 TextOutW，扫描字形实际墨迹 bbox（alpha≥0x10），清位图后按 (居中目标-墨迹中心) 偏移重绘，实现按字形边界框的真水平+垂直居中（非行高估算）；CreateFontW quality=ANTIALIASED_QUALITY 消除模糊；DPI 桶尺寸由 16/24/32 离散改 `dpi*16/96` 连续取整 clamp(16,48)，125%/150% 缩放不再复用 16px 位图拉伸致糊 | 真机视觉缺陷：文本不居中且模糊 |
| D-44 | **卫生项（v0.3.3）**：① 启动空 warnings 不推 ConfigReloaded（补回 is_empty 守卫）；② autostart 注册表持续写失败：pump 改锁外同步（提出 target→锁外 registry I/O→成功更新 state/失败回写配置+推 Alert 警告），消除每 tick 全量 refresh_ui；③ P3-1 右键菜单闪烁：refresh_ui 增菜单签名（设备/电量/勾选态拼接），签名不变跳过 build_menu/set_menu；④ P3-2 WM_ACTIVATE 判定 `& 0xFFFF != 0` 覆盖 WA_CLICKACTIVE。D-38 P2-2 由 D-41 取代关闭 | 上轮 b1d776a 遗留 + 本轮顺手修（改动面允许） |


| D-45 | **轮询间隔默认与档位收敛（v0.3.4 #2）**：默认 `poll_interval_secs` 300→600（电量变化约 1%–2%/小时，600s 丢失信息≤0.2%；休眠唤醒已是被动事件驱动不受影响；低电提醒在阈值 ≥5% 时 10 分钟级延迟可接受）；档位 15/30/60/120/300 → **60/300/600/900/1800**（过短档位与零耗电理念相悖，D-33 同源约束）；配置文件里旧档值经 `normalize_poll_secs` 先钳 [60,1800] 再吸附最近档（并列取更低，护电池）；设置窗口档位文案与耗电提示更新 | 组长定向：真机反馈 #2 |
| D-46 | **低电闪烁根因（v0.3.4 #3，P1）**：相位翻转/重绘调度/触发判定本身正常，坏在「提亮帧」用乘法提亮 `v×(1+k)`——饱和通道被钳回原值（brighten((255,0,0),0.4)=(255,0,0)）、暗色通道变化低于感知阈值，常态帧与提亮帧逐像素相同或差异不可见 → 闪烁"未生效"。修复：提亮帧改 `flash_rgb` 插值——常规色向白插值 `v+(255-v)×k`（每通道必有可感知增量）；近白色（与白对比度 ≤1.2）反向向深灰 0x1B 插值，保证任意颜色两帧有可见差异。状态机抽出 `blink.rs`（BlinkState：相位/低电判定/单击停闪/解除重新武装），逻辑级测试含 AppCore 集成（低电两次去抖→blink、单击停、充电即解除、回滞解除）；充电抑制闪烁保留（D-30 原设计：充电设备外部供电，LowBatteryTracker 充电即复位） | 真机反馈 #3「闪烁好像没生效」+ 渲染数学复核 |
| D-47 | **自定义背景色差异根因（v0.3.4 #1，P1）**：tray-icon 0.26.1 `Icon::from_rgba` 内部走 `CreateIcon`——非预乘 32bpp 数据被当 DDB 颜色平面（Win32 只对 DIB 尊重 32bpp alpha 且要求预乘 BGRA），且 AND 掩码按逐像素整字节填充（规范是 1bpp 压缩 MSB-first）→ alpha 被曲解、透明像素污染 RGB、自定义背景发灰发暗（与质检 CR1 同源）。修复：自建 `rgba_to_hicon` = CreateDIBSection 预乘 BGRA（(v×a+127)/255）+ CreateBitmap 1bpp 规范掩码 + CreateIconIndirect，经 `Icon::from_handle` 移交 tray-icon；设置窗口 draw_dot/预览同类 CreateBitmap DDB 一并改预乘 DIB（`bgra_dib_premultiplied`）。像素级回归测试：纯红 (255,0,0) 背景断言渲染像素 RGB=[255,0,0]、预乘 BGRA=[0,0,255,255]（同时拦字节序颠倒与 alpha 污染）+ 掩码位打包测试 | 真机反馈 #1 + CR1；Win32 语义证据：DIB/DDB alpha 处理差异 |
| D-48 | **图标文本/背景大小自定义（v0.3.4 #4）**：新键 `icon_text_scale`（60..=140%，默认 100）/`icon_bg_scale`（60..=100%，默认 100；上限 100 是几何约束——内切圆/方块 >100% 必溢出位图）；渲染几何：背景块 block=位图×bs%、圆角半径随 block、文本高度上限 block×15/16 再按宽度比例收缩，两遍墨迹 bbox 居中保留（缩小仍真居中）；100% 档与 v0.3.3 逐字节一致。设置窗口新增「图标大小」组：两滑块 + 实时预览 + 即时持久化 TOML + 热重载（pump 对账纳入两键，缓存 rebuild 检测）；渲染测试：缩放矩阵（3×3×2 形状×文本档）断言墨迹不出背景块、背景块不出位图 | 真机反馈 #4 可读性诉求 |
| D-49 | **P3-2/P3-3 顺带修（v0.3.4）**：P3-2 档数自动增长由「+30 递增」（顶档只覆盖 100%，如 20/50/80/99）改为在 [上一档+1, 99] 区间按剩余档数均分（20/50 → 3 档补 75，覆盖 76-100）；P3-3 预览用色与托盘对齐——分档启用且非数字文本（?/--）用 Gray（原用 Default），"满" 按 100 参与分档（原 parse 失败回退错档） | 上轮质检 P3 遗留，改动面允许顺带 |
| D-50 | **README 标题连写（v0.3.4）**：仓库根 README.md 标题「Logi VoltTray」→「LogiVoltTray」（D-39 命名统一的漏网处；正文他处已连写） | 上轮 P3-1 卫生项 |
