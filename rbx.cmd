@echo off
setlocal EnableExtensions EnableDelayedExpansion
set "CLI="

if not "%RENIUM_CLI%"=="" if exist "%RENIUM_CLI%" set "CLI=%RENIUM_CLI%"

if "%CLI%"=="" (
  for %%I in (renium.exe) do if not "%%~$PATH:I"=="" set "CLI=%%~$PATH:I"
)

if "%CLI%"=="" if exist "%~dp0renium.exe" set "CLI=%~dp0renium.exe"
if "%CLI%"=="" if exist "%~dp0bin\renium.exe" set "CLI=%~dp0bin\renium.exe"
if "%CLI%"=="" if exist "%~dp0tools\renium\target\release\renium.exe" set "CLI=%~dp0tools\renium\target\release\renium.exe"

if "%CLI%"=="" (
  echo Renium CLI not found. Install renium.exe on PATH or set RENIUM_CLI to its full path.
  exit /b 9009
)

set "RUNNER=%~dp0rbx-run.ps1"
if not exist "%RUNNER%" if exist "%~dp0tools\renium\rbx-run.ps1" set "RUNNER=%~dp0tools\renium\rbx-run.ps1"

if /I "%~1"=="l" (
  if "%~2"=="" (
    echo Usage: rbx l "print('hi')"
    exit /b 2
  )
  "%CLI%" lx -e "%~2"
  exit /b %ERRORLEVEL%
)

if /I "%~1"=="lf" (
  if "%~2"=="" (
    echo Usage: rbx lf path\to\script.luau
    exit /b 2
  )
  "%CLI%" lx -f "%~2"
  exit /b %ERRORLEVEL%
)

if /I "%~1"=="lc" (
  if "%~2"=="" (
    echo Usage: rbx lc "print('hi')" [player]
    exit /b 2
  )
  if "%~3"=="" (
    "%CLI%" lx -c -e "%~2"
  ) else (
    "%CLI%" lx --player "%~3" -e "%~2"
  )
  exit /b %ERRORLEVEL%
)

if /I "%~1"=="r" (
  if "%~2"=="" (
    echo Usage: rbx r "print('hi')"
    exit /b 2
  )
  if not exist "%RUNNER%" (
    echo rbx-run.ps1 not found next to rbx.cmd or under tools\renium.
    exit /b 2
  )
  powershell -NoProfile -ExecutionPolicy Bypass -File "%RUNNER%" -Cli "%CLI%" -Code "%~2"
  exit /b %ERRORLEVEL%
)

if /I "%~1"=="rf" (
  if "%~2"=="" (
    echo Usage: rbx rf path\to\script.luau
    exit /b 2
  )
  if not exist "%RUNNER%" (
    echo rbx-run.ps1 not found next to rbx.cmd or under tools\renium.
    exit /b 2
  )
  powershell -NoProfile -ExecutionPolicy Bypass -File "%RUNNER%" -Cli "%CLI%" -File "%~2"
  exit /b %ERRORLEVEL%
)

if /I "%~1"=="rc" (
  if "%~2"=="" (
    echo Usage: rbx rc "print('hi')"
    exit /b 2
  )
  if not exist "%RUNNER%" (
    echo rbx-run.ps1 not found next to rbx.cmd or under tools\renium.
    exit /b 2
  )
  powershell -NoProfile -ExecutionPolicy Bypass -File "%RUNNER%" -Cli "%CLI%" -Client -Code "%~2"
  exit /b %ERRORLEVEL%
)

if /I "%~1"=="rcf" (
  if "%~2"=="" (
    echo Usage: rbx rcf path\to\script.luau
    exit /b 2
  )
  if not exist "%RUNNER%" (
    echo rbx-run.ps1 not found next to rbx.cmd or under tools\renium.
    exit /b 2
  )
  powershell -NoProfile -ExecutionPolicy Bypass -File "%RUNNER%" -Cli "%CLI%" -Client -File "%~2"
  exit /b %ERRORLEVEL%
)

if /I "%~1"=="rw" (
  if "%~2"=="" (
    echo Usage: rbx rw "print('hi')" [consoleWaitSeconds]
    exit /b 2
  )
  set "WAIT=%~3"
  if "%WAIT%"=="" set "WAIT=6"
  if not exist "%RUNNER%" (
    echo rbx-run.ps1 not found next to rbx.cmd or under tools\renium.
    exit /b 2
  )
  powershell -NoProfile -ExecutionPolicy Bypass -File "%RUNNER%" -Cli "%CLI%" -ConsoleWaitSeconds !WAIT! -Code "%~2"
  exit /b %ERRORLEVEL%
)

if /I "%~1"=="rcw" (
  if "%~2"=="" (
    echo Usage: rbx rcw "print('hi')" [consoleWaitSeconds]
    exit /b 2
  )
  set "WAIT=%~3"
  if "%WAIT%"=="" set "WAIT=6"
  if not exist "%RUNNER%" (
    echo rbx-run.ps1 not found next to rbx.cmd or under tools\renium.
    exit /b 2
  )
  powershell -NoProfile -ExecutionPolicy Bypass -File "%RUNNER%" -Cli "%CLI%" -Client -ConsoleWaitSeconds !WAIT! -Code "%~2"
  exit /b %ERRORLEVEL%
)

if /I "%~1"=="lcf" (
  if "%~2"=="" (
    echo Usage: rbx lcf path\to\script.luau
    exit /b 2
  )
  "%CLI%" lx -c -f "%~2"
  exit /b %ERRORLEVEL%
)

if /I "%~1"=="c" (
  powershell -NoProfile -ExecutionPolicy Bypass -Command "& { $env:RENIUM_CLI = '%CLI%'; $json = & '%CLI%' co -n 1 | ConvertFrom-Json; if ($json.entries.Count -gt 0) { [string]$json.entries[0].message -replace '\r?\n', ' | ' } }"
  exit /b %ERRORLEVEL%
)

if /I "%~1"=="cl" (
  set "LIMIT=%~2"
  if "%LIMIT%"=="" set "LIMIT=20"
  powershell -NoProfile -ExecutionPolicy Bypass -Command "& { $env:RENIUM_CLI = '%CLI%'; $json = & '%CLI%' co -n !LIMIT! | ConvertFrom-Json; foreach ($entry in $json.entries) { $msg = [string]$entry.message -replace '\r?\n', ' | '; '{0}: {1}' -f $entry.type, $msg } }"
  exit /b %ERRORLEVEL%
)

if /I "%~1"=="ps" (
  if "%~2"=="" (
    "%CLI%" play -s
  ) else (
    "%CLI%" play -s --players %~2
  )
  exit /b %ERRORLEVEL%
)

if /I "%~1"=="px" (
  "%CLI%" play -x
  exit /b %ERRORLEVEL%
)

if /I "%~1"=="pl" (
  "%CLI%" play -s
  if errorlevel 1 exit /b %ERRORLEVEL%
  timeout /t 3 /nobreak >nul
  "%CLI%" play -x
  exit /b %ERRORLEVEL%
)

if /I "%~1"=="status" (
  "%CLI%" lx -e "local r=game:GetService('RunService') local s=game:GetService('StudioTestService') return { IsRunning=tostring(r:IsRunning()), IsEdit=tostring(r:IsEdit()), RunState=tostring(r.RunState), EditModeActive=tostring((s :: any).EditModeActive) }"
  exit /b %ERRORLEVEL%
)

"%CLI%" %*
exit /b %ERRORLEVEL%
