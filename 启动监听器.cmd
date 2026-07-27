@echo off
setlocal
cd /d "%~dp0"
if exist "%~dp0ClaudeAutoContinue.exe" (
  start "" "%~dp0ClaudeAutoContinue.exe"
  exit /b 0
)
where pythonw.exe >nul 2>nul
if %errorlevel%==0 (
  start "" pythonw.exe "%~dp0app_web.pyw"
  exit /b 0
)
where python.exe >nul 2>nul
if %errorlevel%==0 (
  start "" python.exe "%~dp0app_web.pyw"
  exit /b 0
)
msg * 未找到 ClaudeAutoContinue.exe 或 Python 运行环境。
exit /b 1
