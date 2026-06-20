# touchHLE Zombie Farm

English | [中文](README.md)

This branch is intended for running Playforge's Zombie Farm / ZFR. It includes compatibility fixes and debugging helpers for Zombie Farm, and those changes are not guaranteed to be suitable for other games.

## Requirements

You need:

- A packaged touchHLE executable.
- Your own Zombie Farm / ZFR IPA file.
- The font assets in `touchHLE_fonts/`.

Use a short path without non-ASCII characters when possible, for example:

```text
D:\Games\touchHLE
C:\touchHLE
```

You can keep your IPA in a fixed folder such as:

```text
.\zombie_farm\ZFR.ipa
```

## Building On Windows

Windows builds require:

- Rust toolchain
- CMake
- Visual Studio 2022 Build Tools with the C++ build tools installed

Use the provided Windows build script:

```bat
.\build_windows.bat release
```

For a debug build:

```bat
.\build_windows.bat debug
```

## Running On Windows

Open Command Prompt or PowerShell in the extracted touchHLE folder, then run:

```bat
.\touchHLE.exe ".\zombie_farm\ZFR.ipa" --device-family="ipad"
```

If your IPA is somewhere else, replace the path with your actual file path:

```bat
.\touchHLE.exe "D:\Games\ZombieFarm\ZFR.ipa" --device-family="ipad"
```

Some versions can also be started with the default device mode:

```bat
.\touchHLE.exe ".\zombie_farm\Zombie_Farm_1.181.ipa"
```

## Time Offset

To shift the in-emulator time forward or backward, set an environment variable before launching the game.

This PowerShell example shifts the in-emulator time forward by 900 seconds, or 15 minutes:

```powershell
$env:TOUCHHLE_TIME_OFFSET_SECONDS="900"
.\touchHLE.exe ".\zombie_farm\ZFR.ipa" --device-family="ipad"
```

The setting only applies to the current PowerShell window. It disappears after you close that window.

To clear the time offset:

```powershell
Remove-Item Env:\TOUCHHLE_TIME_OFFSET_SECONDS
```

## Windows Shortcuts

These shortcuts are mainly for Zombie Farm debugging and troubleshooting:

- `F9`: Zombie Farm only. Quickly completes the current quest queue through the game's own logic and claims the rewards. Press it once after launching the game.
- `F10`: Toggles the visual UI element inspector. Hover with the mouse to inspect UIKit elements. Press `F10` again or press `Esc` to close it.
- `F11`: Dumps the current UI inspection information to the log. Useful for checking view hierarchy, tables, and control state.
- `F12`: Requests debugger entry. This only takes effect when touchHLE is connected to a debugger; otherwise it is ignored.

## Android Is Unsupported

The Android version is currently unsupported and should only be treated as experimental. Known limitations:

- The `TOUCHHLE_TIME_OFFSET_SECONDS` time offset environment variable cannot be used.
- Quest progress cannot be saved.
- The `F9` one-key quest completion shortcut is not available.

If you still want to try the Android version, place the IPA like this:

1. Install the Android APK.
2. Put the IPA file into the app data folder's `touchHLE_apps` directory.

For the default package name, the path is usually:

```text
/sdcard/Android/data/org.touchhle.android/files/touchHLE_apps
```

If you installed an APK with a branding suffix, the package name in this path may be different. Use the actual package name on your device.

3. Open touchHLE.
4. Select the IPA file in the app and start the game.

## Troubleshooting

### The executable is missing

Make sure you extracted the full Windows package. `touchHLE.exe` should be in the same folder as this README.

### The IPA path contains spaces

Wrap paths containing spaces in double quotes:

```bat
.\touchHLE.exe "D:\Games\ZombieFarm\ZFR 1.0.ipa" --device-family="ipad"
```

### iPad layout or assets look wrong

ZFR usually works best with the iPad device argument:

```bat
--device-family="ipad"
```

Full example:

```bat
.\touchHLE.exe ".\zombie_farm\ZFR.ipa" --device-family="ipad"
```
