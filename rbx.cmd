@echo off
setlocal EnableExtensions DisableDelayedExpansion
set "RENIUM_AGENT_CLI=1"
set "CLI="

if not "%RENIUM_CLI%"=="" if exist "%RENIUM_CLI%" set "CLI=%RENIUM_CLI%"
if "%CLI%"=="" if exist "%~dp0renium.exe" set "CLI=%~dp0renium.exe"
if "%CLI%"=="" if exist "%~dp0bin\renium.exe" set "CLI=%~dp0bin\renium.exe"
if "%CLI%"=="" if exist "%LOCALAPPDATA%\Renium\bin\renium.exe" set "CLI=%LOCALAPPDATA%\Renium\bin\renium.exe"
if "%CLI%"=="" if exist "%~dp0tools\renium\target\release\renium.exe" set "CLI=%~dp0tools\renium\target\release\renium.exe"
if "%CLI%"=="" for %%I in (renium.exe) do if not "%%~$PATH:I"=="" set "CLI=%%~$PATH:I"

if "%CLI%"=="" (
  echo Renium CLI not found. Install renium.exe on PATH or set RENIUM_CLI to its full path.
  exit /b 9009
)

"%CLI%" %*
exit /b %ERRORLEVEL%
