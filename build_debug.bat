@echo off
setlocal

cd /d "%~dp0"
echo Building Y2M Chat (debug)...
cargo build --manifest-path "%~dp0Cargo.toml"

if errorlevel 1 (
    echo.
    echo Debug build failed.
    exit /b 1
)

echo.
echo Debug build succeeded:
echo %~dp0target\debug\y2m.exe
exit /b 0
