[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$InstallerScript,
    [Parameter(Mandatory = $true)]
    [string]$Version,
    [Parameter(Mandatory = $true)]
    [string]$OutputPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$installer = (Resolve-Path -LiteralPath $InstallerScript).Path
$output = [IO.Path]::GetFullPath($OutputPath)
$installerPayload = [Convert]::ToBase64String([IO.File]::ReadAllBytes($installer))
$bootstrap = @'
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$LauncherPath,
    [Parameter(Mandatory = $true)][string]$Version
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Invoke-Installer {
    param([string]$Path)
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Path -Version $Version -Interactive
    $script:InstallerExitCode = $LASTEXITCODE
}

function Add-ArchiveCandidates {
    param([string]$Text, [Collections.Generic.List[string]]$Candidates)
    if ([string]::IsNullOrWhiteSpace($Text)) {
        return
    }
    if ($Text.StartsWith("file:///", [StringComparison]::OrdinalIgnoreCase)) {
        try {
            $localPath = ([uri]$Text).LocalPath
            $archiveMatch = [regex]::Match(
                $localPath,
                '^(?<archive>[A-Z]:\\.*?\.zip)(?:\\|$)',
                'IgnoreCase'
            )
            if ($archiveMatch.Success) {
                $archivePath = $archiveMatch.Groups["archive"].Value
                if ((Test-Path -LiteralPath $archivePath -PathType Leaf) -and
                    -not $Candidates.Contains($archivePath)) {
                    $Candidates.Add($archivePath)
                }
            }
        }
        catch {
        }
    }
    foreach ($match in [regex]::Matches(
        $Text,
        '(?i)(?:"(?<quoted>[^"]+\.zip)"|(?<plain>[A-Z]:\\[^\s"]+\.zip))'
    )) {
        $candidate = if ($match.Groups["quoted"].Success) {
            $match.Groups["quoted"].Value
        }
        else {
            $match.Groups["plain"].Value
        }
        if ((Test-Path -LiteralPath $candidate -PathType Leaf) -and
            -not $Candidates.Contains($candidate)) {
            $Candidates.Add($candidate)
        }
    }
}

function Test-ReniumArchive {
    param([string]$Path)
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    try {
        $archive = [IO.Compression.ZipFile]::OpenRead($Path)
        try {
            $names = @($archive.Entries | ForEach-Object { $_.FullName.Replace('\', '/').ToLowerInvariant() })
            return $names -contains "install.ps1" -and
                $names -contains "renium.exe" -and
                $names -contains "renium.rbxm" -and
                @($names | Where-Object { $_ -like "renium-$Version-win32-*.vsix" }).Count -gt 0
        }
        finally {
            $archive.Dispose()
        }
    }
    catch {
        return $false
    }
}

function Find-ReniumArchive {
    $candidates = New-Object 'System.Collections.Generic.List[string]'
    $directoryName = Split-Path -Leaf (Split-Path -Parent $LauncherPath)
    $rarMatch = [regex]::Match($directoryName, '^Rar\$.*?a(?<pid>\d+)\.', 'IgnoreCase')
    if ($rarMatch.Success) {
        $archiveProcess = Get-CimInstance Win32_Process -Filter (
            "ProcessId = " + $rarMatch.Groups["pid"].Value
        ) -ErrorAction SilentlyContinue
        if ($null -ne $archiveProcess) {
            Add-ArchiveCandidates $archiveProcess.CommandLine $candidates
        }
    }

    $processId = $PID
    for ($depth = 0; $depth -lt 8 -and $processId -gt 0; $depth++) {
        $process = Get-CimInstance Win32_Process -Filter "ProcessId = $processId" -ErrorAction SilentlyContinue
        if ($null -eq $process) {
            break
        }
        Add-ArchiveCandidates $process.CommandLine $candidates
        $processId = [int]$process.ParentProcessId
    }

    Get-CimInstance Win32_Process -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -in @("WinRAR.exe", "7zFM.exe", "Bandizip.exe") } |
        ForEach-Object { Add-ArchiveCandidates $_.CommandLine $candidates }

    try {
        $shell = New-Object -ComObject Shell.Application
        foreach ($window in @($shell.Windows())) {
            Add-ArchiveCandidates ([uri]::UnescapeDataString([string]$window.LocationURL)) $candidates
        }
    }
    catch {
    }

    $expectedName = "renium-$Version-windows-" +
        $(if ([Runtime.InteropServices.RuntimeInformation]::OSArchitecture -eq "Arm64") { "arm64" } else { "x64" }) +
        ".zip"
    foreach ($candidate in @($candidates | Where-Object {
        [IO.Path]::GetFileName($_) -ieq $expectedName
    })) {
        if (Test-ReniumArchive $candidate) {
            return $candidate
        }
    }
    return $null
}

$sibling = Join-Path (Split-Path -Parent $LauncherPath) "install.ps1"
if (Test-Path -LiteralPath $sibling -PathType Leaf) {
    Invoke-Installer $sibling
    exit $script:InstallerExitCode
}

$temporary = Join-Path $env:TEMP ("renium-zip-install-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $temporary | Out-Null
try {
    $archive = Find-ReniumArchive
    if ($null -ne $archive) {
        Write-Host "Loading Renium from $archive"
        Expand-Archive -LiteralPath $archive -DestinationPath $temporary
        $extractedInstaller = Join-Path $temporary "install.ps1"
        if (-not (Test-Path -LiteralPath $extractedInstaller -PathType Leaf)) {
            throw "The Renium ZIP does not contain install.ps1"
        }
        Invoke-Installer $extractedInstaller
        exit $script:InstallerExitCode
    }

    $content = [IO.File]::ReadAllText($LauncherPath)
    $startMarker = ":__RENIUM_INSTALLER_PAYLOAD__"
    $endMarker = ":__RENIUM_BOOTSTRAP_PAYLOAD__"
    $start = $content.LastIndexOf($startMarker)
    $end = $content.LastIndexOf($endMarker)
    if ($start -lt 0 -or $end -le $start) {
        throw "Installer payload is missing"
    }
    $fallback = Join-Path $temporary "install.ps1"
    $encoded = $content.Substring($start + $startMarker.Length, $end - $start - $startMarker.Length)
    [IO.File]::WriteAllBytes($fallback, [Convert]::FromBase64String($encoded))
    Invoke-Installer $fallback
    exit $script:InstallerExitCode
}
finally {
    Remove-Item -LiteralPath $temporary -Recurse -Force -ErrorAction SilentlyContinue
}
'@

function Split-Payload {
    param([string]$Payload)
    for ($offset = 0; $offset -lt $Payload.Length; $offset += 76) {
        $Payload.Substring($offset, [Math]::Min(76, $Payload.Length - $offset))
    }
}
$bootstrapPayload = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($bootstrap))
$installerLines = @(Split-Payload $installerPayload)
$bootstrapLines = @(Split-Payload $bootstrapPayload)
$launcher = @'
@echo off
setlocal
title Install Renium
set "renium_bootstrap=%TEMP%\renium-bootstrap-%RANDOM%-%RANDOM%.ps1"
set "RENIUM_LAUNCHER=%~f0"
powershell.exe -NoProfile -ExecutionPolicy Bypass -Command "$s=[IO.File]::ReadAllText($env:RENIUM_LAUNCHER);$m=':__RENIUM_BOOTSTRAP_PAYLOAD__';$i=$s.LastIndexOf($m);if($i -lt 0){throw 'Installer bootstrap is missing'};$b=[Convert]::FromBase64String($s.Substring($i+$m.Length));[IO.File]::WriteAllBytes('%renium_bootstrap%',$b)"
if errorlevel 1 goto bootstrap_failed

echo Installing Renium...
echo.
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%renium_bootstrap%" -LauncherPath "%~f0" -Version "{{VERSION}}"
set "result=%ERRORLEVEL%"
del /q "%renium_bootstrap%" >nul 2>&1
echo.
if "%result%"=="3" (
  echo Installation cancelled.
  pause
  exit /b 0
)
if not "%result%"=="0" (
  echo Installation failed. The error is shown above.
  pause
  exit /b %result%
)
echo Installation complete.
echo Open a new terminal to use the renium command.
echo Restart your editor and Roblox Studio.
pause
exit /b 0

:bootstrap_failed
echo.
echo Installation could not start.
pause
exit /b 1

:__RENIUM_INSTALLER_PAYLOAD__
'@.Replace("{{VERSION}}", $Version)

New-Item -ItemType Directory -Path ([IO.Path]::GetDirectoryName($output)) -Force | Out-Null
[IO.File]::WriteAllText(
    $output,
    $launcher +
        ($installerLines -join "`r`n") +
        "`r`n:__RENIUM_BOOTSTRAP_PAYLOAD__`r`n" +
        ($bootstrapLines -join "`r`n") +
        "`r`n",
    [Text.Encoding]::ASCII
)
