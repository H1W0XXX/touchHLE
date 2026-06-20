# touchHLE Zombie Farm

[English](README.en.md) | 中文

这个分支用于运行 Playforge 的 Zombie Farm / ZFR。它包含了一些面向 Zombie Farm 的兼容性修复和调试辅助，不保证这些改动适合其他游戏。

## 准备

你需要准备：

- 打包好的 touchHLE 可执行文件。
- 你自己的 Zombie Farm / ZFR IPA 文件。
- `touchHLE_fonts/` 目录中的字体资源。

建议把 touchHLE 放在非中文、路径较短的目录，例如：

```text
D:\Games\touchHLE
C:\touchHLE
```

也建议把 IPA 放在一个固定目录，例如：

```text
.\zombie_farm\ZFR.ipa
```

## Windows 构建

Windows 下需要：

- Rust toolchain
- CMake
- Visual Studio 2022 Build Tools，安装 C++ 构建工具

仓库提供了 Windows 构建脚本：

```bat
.\build_windows.bat release
```

调试构建可以运行：

```bat
.\build_windows.bat debug
```

## Windows 启动

在 touchHLE 解压目录打开 Command Prompt 或 PowerShell，然后运行：

```bat
.\touchHLE.exe ".\zombie_farm\ZFR.ipa" --device-family="ipad"
```

如果 IPA 放在其他目录，替换成你的实际路径：

```bat
.\touchHLE.exe "D:\Games\ZombieFarm\ZFR.ipa" --device-family="ipad"
```

部分版本也可以用默认设备模式启动：

```bat
.\touchHLE.exe ".\zombie_farm\Zombie_Farm_1.181.ipa"
```

## 时间偏移

如果需要让模拟器内时间向未来或过去偏移，可以在 PowerShell 中先设置环境变量。

下面的示例会让模拟器内时间向未来偏移 900 秒，也就是 15 分钟：

```powershell
$env:TOUCHHLE_TIME_OFFSET_SECONDS="900"
.\touchHLE.exe ".\zombie_farm\ZFR.ipa" --device-family="ipad"
```

这个设置只对当前 PowerShell 窗口有效。关闭窗口后会失效。

取消时间偏移：

```powershell
Remove-Item Env:\TOUCHHLE_TIME_OFFSET_SECONDS
```

## Windows 快捷键

这些快捷键主要用于 Zombie Farm 调试和排查：

- `F9`：仅对 Zombie Farm 生效。按游戏逻辑快速完成当前任务队列中的任务并领取奖励。每次启动后只需要按一次。
- `F10`：开关可视化界面元素检查器。打开后可以用鼠标查看当前 UIKit 元素，再按一次 `F10` 或按 `Esc` 关闭。
- `F11`：把当前界面检查信息输出到日志，适合排查界面层级、表格、控件状态。
- `F12`：请求进入调试器。只有在 touchHLE 已连接调试器时才会真正进入，否则会被忽略。

## Android 版本不受支持

Android 版本目前不受支持，只能作为实验性版本尝试。已知限制：

- 无法使用 `TOUCHHLE_TIME_OFFSET_SECONDS` 时间偏移环境变量。
- 无法保存任务进度。
- 无法使用 `F9` 一键完成任务。

如果仍然需要尝试 Android 版本，可以按下面的方式放置 IPA：

1. 安装 Android APK。
2. 把 IPA 放到应用数据目录下的 `touchHLE_apps` 文件夹。

默认包名的路径通常是：

```text
/sdcard/Android/data/org.touchhle.android/files/touchHLE_apps
```

如果你安装的是带 branding 后缀的 APK，目录中的包名可能不同，请按设备上的实际包名调整。

3. 打开 touchHLE。
4. 在应用里选择 IPA 文件启动游戏。

## 常见问题

### 找不到可执行文件

请确认你已经解压完整的 Windows 版压缩包。`touchHLE.exe` 应该和 README 位于同一个目录。

### IPA 路径包含空格

路径中有空格时，请用英文双引号包住路径：

```bat
.\touchHLE.exe "D:\Games\ZombieFarm\ZFR 1.0.ipa" --device-family="ipad"
```

### iPad 画面或资源不正确

ZFR 通常建议带上 iPad 设备参数：

```bat
--device-family="ipad"
```

完整示例：

```bat
.\touchHLE.exe ".\zombie_farm\ZFR.ipa" --device-family="ipad"
```
