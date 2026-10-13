# LogiVoltTray

Windows 系统托盘上的罗技设备电量显示器：在托盘图标上直接画出每个键盘 / 鼠标的实时电量数字，右键菜单一键开关「开机启动」。

- 仓库：https://github.com/isxulong/Logi-VoltTray
- 打包产物文件名：`LogiVoltTray.exe`

## 功能列表

- **托盘电量直显**：每个检测到的罗技设备一个托盘图标，图标上直接绘制电量百分比数字；鼠标蓝色、键盘绿色，低电量自动转橙 / 红。
- **广覆盖的读取通道**：HID++ 2.0（统一接收器）、老款 HID++ 1.0 键鼠、蓝牙直连设备、有线设备均可读取。
- **真实设备名**：显示「MX Keys」这类真实型号名，而不是笼统的「键盘 / 鼠标」；粗档位电量显示为「约 90%」。
- **开机启动开关**：右键菜单勾选 / 取消即可，以注册表 Run 键（HKCU，免管理员权限）为唯一事实源；外部在系统设置里删除自启项不会被程序复活，程序移动 / 改名后启动时自动修正注册表路径。
- **设备诊断报告**：右键菜单一键生成诊断报告，把本机可见的罗技 HID 通道、各设备槽位应答、可用电量特征全部导出，方便排查「读不到」的设备。
- **交互**：左键点击图标立即重新检测设备；双击程序即启动；退出不卡界面。
- **多实例防护**：同名互斥锁保证只运行一个实例。

## 运行

依赖：Windows、Python 3.13、[hidapi](https://pypi.org/project/hidapi/)（`pip install hidapi`）。

```
python logi_tray.py
```

命令行直接生成诊断报告：

```
python logi_report.py            # 打印到控制台
LogiVoltTray.exe --report        # 打包版：生成报告文件并打开所在目录
```

## 打包

```
pip install pyinstaller
pyinstaller --onefile --windowed --name LogiVoltTray logi_tray.py
```

产物为 `dist/LogiVoltTray.exe`，双击即用。

## 测试

```
python -m unittest discover -s tests
```

80 项单测覆盖：开机启动注册表逻辑（内存 winreg）、核心服务逻辑、HID++ 协议纯逻辑、报告生成、托盘菜单与同步逻辑、协议冻结核验。

> 协议冻结核验（`tests/test_protocol_freeze.py`）需要在与仓库根目录同级的 `red/` 目录中放入红版基线 `logi_hidpp.py`（与 T1 基线逐字节一致），作为 29 个协议常量 + 11 个协议函数的比对参照。

## 致谢

本项目基于原程序「罗技电量Pro」逆向重建而来：从原始 exe 的反汇编产物出发重建全部源码，再经红蓝对抗评审（两方独立优化 + 23 项裁决合并）修复了 HID 通道泄漏、注册表复活、托盘认领越权等一批原程序缺陷，并对 HID++ 协议实现做字节级冻结保护。感谢原程序作者的工作。

## 许可证

仅供学习与研究使用。罗技（Logitech）相关商标归其所有者所有，本项目与罗技公司无任何关联。
