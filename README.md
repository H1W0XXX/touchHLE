# touchHLE zombiefarm

这个分支只用于跑 Playforge 的 Zombie Farm。

分支名：`zombiefarm`

## 构建环境

需要正常的 touchHLE 构建环境：

- Rust toolchain
- CMake
- `touchHLE_fonts/` 里的字体资源
- 平台原生构建依赖

Windows 下额外需要：

- Visual Studio Build Tools

仓库里提供了一个方便用的构建脚本：

```bat
.\build_windows.bat release
```

## 启动示例

Zombie Farm 1.181：

```bat
.\target\release\touchHLE.exe ".\zombie_farm\Zombie_Farm_1.181.ipa"
```

ZFR06 iPad 模式：

```bat
.\target\release\touchHLE.exe ".\zombie_farm\ZFR06.ipa" --device-family="ipad"
```

## 说明

这个分支包含的是为了 Zombie Farm 做的兼容性修改，不保证对别的游戏没有副作用。
