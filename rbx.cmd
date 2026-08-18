@echo off
setlocal EnableExtensions DisableDelayedExpansion
set "CLI="

if not "%RENIUM_CLI%"=="" if exist "%RENIUM_CLI%" set "CLI=%RENIUM_CLI%"
if "%CLI%"=="" if exist "%~dp0renium.exe" set "CLI=%~dp0renium.exe"
if "%CLI%"=="" if exist "%~dp0bin\renium.exe" set "CLI=%~dp0bin\renium.exe"
if "%CLI%"=="" if exist "%LOCALAPPDATA%\Renium\bin\renium.exe" set "CLI=%LOCALAPPDATA%\Renium\bin\renium.exe"
if "%CLI%"=="" if exist "%~dp0tools\renium\target\release\renium.exe" set "CLI=%~dp0tools\renium\target\release\renium.exe"

if "%CLI%"=="" (
  for %%I in (renium.exe) do if not "%%~$PATH:I"=="" set "CLI=%%~$PATH:I"
)

if "%CLI%"=="" (
  echo Renium CLI not found. Install renium.exe on PATH or set RENIUM_CLI to its full path.
  exit /b 9009
)

set "RUNNER=%~dp0rbx-run.ps1"
if not exist "%RUNNER%" if exist "%~dp0tools\renium\rbx-run.ps1" set "RUNNER=%~dp0tools\renium\rbx-run.ps1"
set "RENIUM_CLI=%CLI%"

if /I "%~1"=="lf" (
  if "%~2"=="" (
    echo Usage: rbx lf path\to\script.luau
    exit /b 2
  )
  "%CLI%" lx -f "%~2"
  goto :exitcode
)

if /I "%~1"=="wait" goto :wait

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
  goto :exitcode
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
  goto :exitcode
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
  goto :exitcode
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
  goto :exitcode
)

if /I "%~1"=="rw" (
  if "%~2"=="" (
    echo Usage: rbx rw "print('hi')" [consoleWaitSeconds]
    exit /b 2
  )
  if not exist "%RUNNER%" (
    echo rbx-run.ps1 not found next to rbx.cmd or under tools\renium.
    exit /b 2
  )
  if "%~3"=="" (
    powershell -NoProfile -ExecutionPolicy Bypass -File "%RUNNER%" -Cli "%CLI%" -ConsoleWaitSeconds 6 -Code "%~2"
  ) else (
    powershell -NoProfile -ExecutionPolicy Bypass -File "%RUNNER%" -Cli "%CLI%" -ConsoleWaitSeconds "%~3" -Code "%~2"
  )
  goto :exitcode
)

if /I "%~1"=="rcw" (
  if "%~2"=="" (
    echo Usage: rbx rcw "print('hi')" [consoleWaitSeconds]
    exit /b 2
  )
  if not exist "%RUNNER%" (
    echo rbx-run.ps1 not found next to rbx.cmd or under tools\renium.
    exit /b 2
  )
  if "%~3"=="" (
    powershell -NoProfile -ExecutionPolicy Bypass -File "%RUNNER%" -Cli "%CLI%" -Client -ConsoleWaitSeconds 6 -Code "%~2"
  ) else (
    powershell -NoProfile -ExecutionPolicy Bypass -File "%RUNNER%" -Cli "%CLI%" -Client -ConsoleWaitSeconds "%~3" -Code "%~2"
  )
  goto :exitcode
)

if /I "%~1"=="lcf" (
  if "%~2"=="" (
    echo Usage: rbx lcf path\to\script.luau
    exit /b 2
  )
  "%CLI%" lx -c -f "%~2"
  goto :exitcode
)

if /I "%~1"=="l" goto :luau
if /I "%~1"=="lc" goto :luau_client

if /I "%~1"=="c" (
  set "RENIUM_CLI=%CLI%"
  powershell -NoProfile -ExecutionPolicy Bypass -Command "& { $cli = $env:RENIUM_CLI; $json = & $cli co -n 1 | ConvertFrom-Json; if ($json.entries.Count -gt 0) { [string]$json.entries[0].message -replace '\r?\n', ' | ' } }"
  goto :exitcode
)

if /I "%~1"=="cl" (
  set "RENIUM_CLI=%CLI%"
  if "%~2"=="" (
    powershell -NoProfile -ExecutionPolicy Bypass -Command "& { $cli = $env:RENIUM_CLI; $json = & $cli co -n 20 | ConvertFrom-Json; foreach ($entry in $json.entries) { $msg = [string]$entry.message -replace '\r?\n', ' | '; '{0}: {1}' -f $entry.type, $msg } }"
  ) else (
    powershell -NoProfile -ExecutionPolicy Bypass -Command "& { $cli = $env:RENIUM_CLI; $json = & $cli co -n $args[0] | ConvertFrom-Json; foreach ($entry in $json.entries) { $msg = [string]$entry.message -replace '\r?\n', ' | '; '{0}: {1}' -f $entry.type, $msg } }" "%~2"
  )
  goto :exitcode
)

if /I "%~1"=="ps" (
  if "%~2"=="" (
    "%CLI%" play -s
  ) else (
    "%CLI%" play -s --players %~2
  )
  goto :exitcode
)

if /I "%~1"=="px" (
  "%CLI%" play -x
  goto :exitcode
)

if /I "%~1"=="pl" (
  "%CLI%" play -s
  if errorlevel 1 goto :exitcode
  timeout /t 3 /nobreak >nul
  "%CLI%" play -x
  goto :exitcode
)

if /I "%~1"=="status" (
  "%CLI%" lx -e "local r=game:GetService('RunService') local s=game:GetService('StudioTestService') return { IsRunning=tostring(r:IsRunning()), IsEdit=tostring(r:IsEdit()), RunState=tostring(r.RunState), EditModeActive=tostring((s :: any).EditModeActive) }"
  goto :exitcode
)

"%CLI%" %*
goto :exitcode

:luau
if "%~2"=="" goto :luau_usage
set "RENIUM_LUAU=%~2"
powershell -NoProfile -ExecutionPolicy Bypass -Command "& $env:RENIUM_CLI lx -e $env:RENIUM_LUAU"
goto :exitcode

:luau_client
if "%~2"=="" goto :luau_client_usage
set "RENIUM_LUAU=%~2"
if "%~3"=="" goto :luau_default_client
set "RENIUM_PLAYER=%~3"
powershell -NoProfile -ExecutionPolicy Bypass -Command "& $env:RENIUM_CLI lx --player $env:RENIUM_PLAYER -e $env:RENIUM_LUAU"
goto :exitcode

:luau_default_client
powershell -NoProfile -ExecutionPolicy Bypass -Command "& $env:RENIUM_CLI lx -c -e $env:RENIUM_LUAU"
goto :exitcode

:luau_usage
echo Usage: rbx l "return game.Name"
exit /b 2

:luau_client_usage
echo Usage: rbx lc "return true" [player]
exit /b 2

:wait
if "%~2"=="" goto :wait_usage
set "WAIT_CONDITION=%~2"
shift
shift
"%CLI%" wait-until "%WAIT_CONDITION%" %1 %2 %3 %4 %5 %6 %7 %8 %9
goto :exitcode

:wait_usage
echo Usage: rbx wait "condition" [-c or --player NAME] [-t SECONDS] [--interval SECONDS]
exit /b 2

:exitcode
exit /b %ERRORLEVEL%
