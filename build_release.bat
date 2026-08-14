@echo off
setlocal

cd /d "%~dp0"
echo Building Y2M Chat (release)...
cargo build --release --manifest-path "%~dp0Cargo.toml"

if errorlevel 1 (
    echo.
    echo Release build failed.
    exit /b 1
)

echo.
echo Release build succeeded:
echo %~dp0target\release\y2m.exe
exit /b 0
