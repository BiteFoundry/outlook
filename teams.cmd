@echo off
chcp 65001 >nul
setlocal
set "ROOT=%~dp0"

if exist "%ROOT%target\release\teams.exe" goto run

echo [teams] release build not found - building... >&2
cargo build --release --manifest-path "%ROOT%Cargo.toml"
if errorlevel 1 exit /b 1

:run
"%ROOT%target\release\teams.exe" %*
exit /b %errorlevel%
