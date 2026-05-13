@echo off
setlocal EnableExtensions

set "SCRIPT_DIR=%~dp0"
cd /d "%SCRIPT_DIR%"

set "PROFILE=release"
if /i "%~1"=="debug" (
    set "PROFILE=debug"
    shift
) else if /i "%~1"=="release" (
    shift
)

set "VSDEVCMD="

if defined VSDEVCMD_PATH (
    if exist "%VSDEVCMD_PATH%" (
        set "VSDEVCMD=%VSDEVCMD_PATH%"
    )
)

if not defined VSDEVCMD (
    set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
    if exist "%VSWHERE%" (
        for /f "usebackq delims=" %%I in (`"%VSWHERE%" -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -find Common7\Tools\VsDevCmd.bat`) do (
            set "VSDEVCMD=%%I"
        )
    )
)

if not defined VSDEVCMD if exist "%ProgramFiles(x86)%\Microsoft Visual Studio\2022\BuildTools\Common7\Tools\VsDevCmd.bat" (
    set "VSDEVCMD=%ProgramFiles(x86)%\Microsoft Visual Studio\2022\BuildTools\Common7\Tools\VsDevCmd.bat"
)
if not defined VSDEVCMD if exist "%ProgramFiles(x86)%\Microsoft Visual Studio\2022\Community\Common7\Tools\VsDevCmd.bat" (
    set "VSDEVCMD=%ProgramFiles(x86)%\Microsoft Visual Studio\2022\Community\Common7\Tools\VsDevCmd.bat"
)
if not defined VSDEVCMD if exist "%ProgramFiles(x86)%\Microsoft Visual Studio\2022\Professional\Common7\Tools\VsDevCmd.bat" (
    set "VSDEVCMD=%ProgramFiles(x86)%\Microsoft Visual Studio\2022\Professional\Common7\Tools\VsDevCmd.bat"
)
if not defined VSDEVCMD if exist "%ProgramFiles(x86)%\Microsoft Visual Studio\2022\Enterprise\Common7\Tools\VsDevCmd.bat" (
    set "VSDEVCMD=%ProgramFiles(x86)%\Microsoft Visual Studio\2022\Enterprise\Common7\Tools\VsDevCmd.bat"
)

if not defined VSDEVCMD (
    echo Failed to find VsDevCmd.bat.
    echo Install Visual Studio 2022 Build Tools with C++ tools, or set VSDEVCMD_PATH.
    exit /b 1
)

set "CMAKE_BIN="
for /d %%D in ("%SCRIPT_DIR%..\.codex_deps\cmake-*-windows-x86_64") do (
    if exist "%%~fD\bin\cmake.exe" (
        set "CMAKE_BIN=%%~fD\bin"
    )
)
if not defined CMAKE_BIN if exist "%SCRIPT_DIR%.codex_deps\cmake-3.29.6-windows-x86_64\bin\cmake.exe" (
    set "CMAKE_BIN=%SCRIPT_DIR%.codex_deps\cmake-3.29.6-windows-x86_64\bin"
)

set "CARGO_FLAGS="
if /i "%PROFILE%"=="release" (
    set "CARGO_FLAGS=--release"
)
set "FORWARD_ARGS=%1 %2 %3 %4 %5 %6 %7 %8 %9"

echo Using VS developer environment: "%VSDEVCMD%"
if defined CMAKE_BIN (
    echo Using bundled CMake: "%CMAKE_BIN%"
)
echo Building touchHLE ^(%PROFILE%^)

call "%VSDEVCMD%" -arch=x64 >nul
if errorlevel 1 exit /b %errorlevel%

if defined CMAKE_BIN (
    set "PATH=%CMAKE_BIN%;%PATH%"
)

cargo build %CARGO_FLAGS% %FORWARD_ARGS%
exit /b %errorlevel%
