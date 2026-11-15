@echo off
chcp 65001 >nul
setlocal
set "ROOT=%~dp0"
set "TEAMS_PERSONA=onenote"
call "%ROOT%teams.cmd" %*
exit /b %errorlevel%
