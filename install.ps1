[CmdletBinding()]
param(
    [string]$Version,
    [switch]$Uninstall,
    [switch]$Interactive
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

$repository = "Superwheat/renium"
$installRoot = Join-Path $env:LOCALAPPDATA "Renium\bin"
$stableLauncherRoot = Join-Path $env:USERPROFILE ".renium\bin"
$stableLauncher = Join-Path $stableLauncherRoot "rbx.cmd"
$stableRunner = Join-Path $stableLauncherRoot "rbx-run.ps1"
$stableExecutable = Join-Path $stableLauncherRoot "rbx.exe"
$stableReniumExecutable = Join-Path $stableLauncherRoot "renium.exe"
$currentPath = [Environment]::GetEnvironmentVariable("Path", "User")
$pathEntries = @($currentPath -split ";" | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })

function Get-NormalizedPathEntry {
    param([string]$Value)
    if ([string]::IsNullOrWhiteSpace($Value)) {
        return $null
    }
    try {
        $expanded = [Environment]::ExpandEnvironmentVariables($Value.Trim().Trim('"'))
        return [IO.Path]::GetFullPath($expanded).TrimEnd('\')
    }
    catch {
        return $null
    }
}

function Install-ReniumCommandAliases {
    $cli = Join-Path $installRoot "renium.exe"
    if (-not (Test-Path -LiteralPath $cli -PathType Leaf)) {
        return
    }
    foreach ($alias in @((Join-Path $installRoot "rbx.exe"))) {
        if (Test-Path -LiteralPath $alias -PathType Leaf) {
            Remove-Item -LiteralPath $alias -Force
        }
        try {
            New-Item -ItemType HardLink -Path $alias -Target $cli -ErrorAction Stop | Out-Null
        }
        catch {
            Copy-Item -LiteralPath $cli -Destination $alias
        }
    }
    foreach ($staleExecutable in @($stableExecutable, $stableReniumExecutable)) {
        if (Test-Path -LiteralPath $staleExecutable -PathType Leaf) {
            Remove-Item -LiteralPath $staleExecutable -Force
        }
    }
}

function Stop-RecordedReniumDaemons {
    $failures = @()
    $discoveryRoot = Join-Path $env:LOCALAPPDATA "Renium"
    if (-not (Test-Path -LiteralPath $discoveryRoot -PathType Container)) {
        return
    }
    foreach ($discovery in @(Get-ChildItem -LiteralPath $discoveryRoot -File -Filter "daemon*.json" -ErrorAction SilentlyContinue)) {
        try {
            $record = Get-Content -LiteralPath $discovery.FullName -Raw | ConvertFrom-Json
            $process = Get-Process -Id ([int]$record.pid) -ErrorAction SilentlyContinue
            if ($null -ne $process) {
                throw "recorded PID $($record.pid) is still running and could not be verified as this daemon"
            }
            Remove-Item -LiteralPath $discovery.FullName -Force
        }
        catch {
            $failures += "$($discovery.Name): $($_.Exception.Message)"
        }
    }
    if ($failures.Count -gt 0) {
        throw "Could not stop every Renium daemon: $($failures -join '; ')"
    }
}

function Assert-ReniumPortsReleased {
    $deadline = [DateTime]::UtcNow.AddSeconds(1)
    do {
        $occupied = @()
        foreach ($port in @(8780, 8781, 8782)) {
            $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, $port)
            try {
                $listener.Start()
            }
            catch {
                $occupied += $port
            }
            finally {
                $listener.Stop()
            }
        }
        if ($occupied.Count -eq 0) {
            return
        }
        Start-Sleep -Milliseconds 50
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Renium daemon ports are still in use: $($occupied -join ', ')"
}

function Stop-ReniumDaemons {
    param([string]$PrimaryCli, [string]$FallbackCli)
    foreach ($cli in @($PrimaryCli, $FallbackCli) | Select-Object -Unique) {
        if ([string]::IsNullOrWhiteSpace($cli) -or
            -not (Test-Path -LiteralPath $cli -PathType Leaf)) {
            continue
        }
        try {
            & $cli daemon stop --all 2>$null
            if ($LASTEXITCODE -eq 0) {
                break
            }
        }
        catch {
        }
    }
    Stop-RecordedReniumDaemons
    Assert-ReniumPortsReleased
}

function Clear-ReniumUpdaterState {
    $root = Join-Path $env:LOCALAPPDATA "Renium"
    foreach ($file in @(
        "update-transaction.json",
        "update-result.json",
        "update-helper-reservation.json"
    )) {
        $path = Join-Path $root $file
        if (Test-Path -LiteralPath $path -PathType Leaf) {
            Remove-Item -LiteralPath $path -Force
        }
    }
    $stages = Join-Path $root "update-stages"
    if (Test-Path -LiteralPath $stages) {
        Remove-Item -LiteralPath $stages -Recurse -Force
    }
    Get-ChildItem -LiteralPath $env:TEMP -File -Filter "renium-update-helper-*" `
        -ErrorAction SilentlyContinue |
        Where-Object {
            $_.Name.EndsWith(".exe", [StringComparison]::OrdinalIgnoreCase) -or
            $_.Name.EndsWith(".result.json", [StringComparison]::OrdinalIgnoreCase)
        } |
        Remove-Item -Force
}

function Get-ProcessStartIdentity {
    param([int]$ProcessId)
    try {
        $process = Get-Process -Id $ProcessId -ErrorAction Stop
        return $process.StartTime.ToUniversalTime().ToFileTimeUtc().ToString()
    }
    catch {
        return $null
    }
}

function Assert-NoActiveReniumUpdateHelper {
    param([string]$Root)
    $path = Join-Path $Root "update-helper-reservation.json"
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        return
    }
    try {
        $reservation = Get-Content -LiteralPath $path -Raw | ConvertFrom-Json
    }
    catch {
        throw "The Renium update helper reservation is malformed."
    }
    if ($null -ne $reservation.helperPid -and
        -not [string]::IsNullOrWhiteSpace([string]$reservation.helperStartIdentity)) {
        $start = Get-ProcessStartIdentity -ProcessId ([int]$reservation.helperPid)
        if ($start -eq [string]$reservation.helperStartIdentity) {
            throw "A Renium update helper is still running."
        }
    }
    if ($null -ne $reservation.parentPid -and
        -not [string]::IsNullOrWhiteSpace([string]$reservation.parentStartIdentity)) {
        $start = Get-ProcessStartIdentity -ProcessId ([int]$reservation.parentPid)
        if ($start -eq [string]$reservation.parentStartIdentity) {
            throw "A Renium update is waiting for its helper to take ownership."
        }
    }
}

function Enter-ReniumLifecycleLock {
    $root = Join-Path $env:LOCALAPPDATA "Renium"
    New-Item -ItemType Directory -Path $root -Force | Out-Null
    $path = Join-Path $root "lifecycle.lock"
    $cleanupPath = Join-Path $root "lifecycle.lock.cleanup"
    $deadline = [DateTime]::UtcNow.AddSeconds(1)
    $startIdentity = Get-ProcessStartIdentity -ProcessId $PID
    if ([string]::IsNullOrWhiteSpace($startIdentity)) {
        throw "Could not read this process's start identity."
    }
    $token = "$PID`t$startIdentity`t$([DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds())"
    $temporary = Join-Path $root ".lifecycle.lock.$PID.$([DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()).tmp"
    [IO.File]::WriteAllText($temporary, $token, [Text.UTF8Encoding]::new($false))
    while ($true) {
        if (Test-Path -LiteralPath $cleanupPath) {
            if ([DateTime]::UtcNow -ge $deadline) {
                Remove-Item -LiteralPath $temporary -Force -ErrorAction SilentlyContinue
                throw "Another Renium lifecycle lock operation is still finishing."
            }
            Start-Sleep -Milliseconds 50
            continue
        }
        try {
            New-Item -ItemType HardLink -Path $path -Target $temporary -ErrorAction Stop | Out-Null
            Remove-Item -LiteralPath $temporary -Force
            try {
                Assert-NoActiveReniumUpdateHelper -Root $root
            }
            catch {
                Remove-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue
                throw
            }
            $script:ReniumLifecycleLockPath = $path
            $script:ReniumLifecycleLockToken = $token
            $env:RENIUM_LIFECYCLE_LOCK_TOKEN = $token
            Register-EngineEvent -SourceIdentifier PowerShell.Exiting -MessageData @{
                Path = $path
                Token = $token
            } -Action {
                try {
                    $data = $event.MessageData
                    if ((Get-Content -LiteralPath $data.Path -Raw -ErrorAction Stop).Trim() -eq $data.Token) {
                        Remove-Item -LiteralPath $data.Path -Force -ErrorAction SilentlyContinue
                    }
                }
                catch {
                }
            } | Out-Null
            return
        }
        catch {
            if (-not (Test-Path -LiteralPath $path)) {
                Remove-Item -LiteralPath $temporary -Force -ErrorAction SilentlyContinue
                throw
            }
            $owner = Join-Path $path "owner"
            $holder = if (Test-Path -LiteralPath $path -PathType Container) {
                Get-Content -LiteralPath $owner -Raw -ErrorAction SilentlyContinue
            } else {
                Get-Content -LiteralPath $path -Raw -ErrorAction SilentlyContinue
            }
            $holderPid = 0
            $holderStart = $null
            $parts = if ($null -ne $holder) { @($holder.Trim() -split "`t") } else { @() }
            $validHolder = $parts.Count -eq 3 -and
                -not [string]::IsNullOrWhiteSpace($parts[1]) -and
                -not [string]::IsNullOrWhiteSpace($parts[2]) -and
                [int]::TryParse($parts[0], [ref]$holderPid)
            if ($validHolder) {
                $holderStart = $parts[1]
            }
            elseif ($null -ne $holder) {
                $legacyParts = @($holder.Trim() -split ":", 2)
                $validHolder = $legacyParts.Count -eq 2 -and
                    -not [string]::IsNullOrWhiteSpace($legacyParts[1]) -and
                    [int]::TryParse($legacyParts[0], [ref]$holderPid)
            }
            if (-not $validHolder -and [DateTime]::UtcNow -lt $deadline) {
                Start-Sleep -Milliseconds 50
                continue
            }
            if (-not $validHolder) {
                Remove-Item -LiteralPath $temporary -Force -ErrorAction SilentlyContinue
                throw "The Renium lifecycle lock is incomplete or malformed."
            }
            $currentStart = Get-ProcessStartIdentity -ProcessId $holderPid
            if ($null -ne $currentStart -and
                ($null -eq $holderStart -or $currentStart -eq $holderStart)) {
                Remove-Item -LiteralPath $temporary -Force -ErrorAction SilentlyContinue
                throw "Another Renium install, update, or uninstall is running."
            }
            try {
                New-Item -ItemType Directory -Path $cleanupPath -ErrorAction Stop | Out-Null
            }
            catch {
                if ([DateTime]::UtcNow -ge $deadline) {
                    Remove-Item -LiteralPath $temporary -Force -ErrorAction SilentlyContinue
                    throw "Another Renium lifecycle lock operation is still finishing."
                }
                Start-Sleep -Milliseconds 50
                continue
            }
            $current = if (Test-Path -LiteralPath $path -PathType Container) {
                Get-Content -LiteralPath $owner -Raw -ErrorAction SilentlyContinue
            }
            else {
                Get-Content -LiteralPath $path -Raw -ErrorAction SilentlyContinue
            }
            if ($null -eq $current -or $current.Trim() -ne $holder.Trim()) {
                Remove-Item -LiteralPath $cleanupPath -Force -ErrorAction SilentlyContinue
                Start-Sleep -Milliseconds 50
                continue
            }
            if (Test-Path -LiteralPath $path -PathType Container) {
                Remove-Item -LiteralPath $path -Recurse -Force
            }
            else {
                Remove-Item -LiteralPath $path -Force
            }
            Remove-Item -LiteralPath $cleanupPath -Force
        }
    }
}

function Exit-ReniumLifecycleLock {
    if ($null -eq $script:ReniumLifecycleLockPath -or $null -eq $script:ReniumLifecycleLockToken) {
        return
    }
    try {
        if ((Get-Content -LiteralPath $script:ReniumLifecycleLockPath -Raw -ErrorAction Stop).Trim() -eq
            $script:ReniumLifecycleLockToken) {
            Remove-Item -LiteralPath $script:ReniumLifecycleLockPath -Force -ErrorAction SilentlyContinue
        }
    }
    catch {
    }
    $script:ReniumLifecycleLockPath = $null
    $script:ReniumLifecycleLockToken = $null
    Remove-Item Env:RENIUM_LIFECYCLE_LOCK_TOKEN -ErrorAction SilentlyContinue
}

function Repair-ReniumCoreInstall {
    $parent = Split-Path -Parent $installRoot
    New-Item -ItemType Directory -Path $parent -Force | Out-Null
    $reserved = @(Get-ChildItem -LiteralPath $parent -Directory -Force -ErrorAction SilentlyContinue |
        Where-Object {
            $_.Name -like ".renium-previous-*" -or
            $_.Name -like ".renium-core-previous-*" -or
            $_.Name -like ".renium-install-*" -or
            $_.Name -like ".renium-core-next-*"
        })
    $backups = @($reserved | Where-Object {
        ($_.Name -like ".renium-previous-*" -or $_.Name -like ".renium-core-previous-*") -and
        (Test-Path -LiteralPath (Join-Path $_.FullName "renium.exe") -PathType Leaf)
    })
    $stages = @($reserved | Where-Object {
        ($_.Name -like ".renium-install-*" -or $_.Name -like ".renium-core-next-*") -and
        (Test-Path -LiteralPath (Join-Path $_.FullName "renium.exe") -PathType Leaf)
    })
    if (-not (Test-Path -LiteralPath $installRoot -PathType Container)) {
        if ($backups.Count -gt 1) {
            throw "Multiple interrupted Renium core backups need manual cleanup in $parent"
        }
        if ($backups.Count -eq 1) {
            Move-Item -LiteralPath $backups[0].FullName -Destination $installRoot
        }
        else {
            if ($stages.Count -gt 1) {
                throw "Multiple interrupted Renium core stages need manual cleanup in $parent"
            }
            if ($stages.Count -eq 1) {
                Move-Item -LiteralPath $stages[0].FullName -Destination $installRoot
            }
        }
    }
    if (Test-Path -LiteralPath $installRoot -PathType Container) {
        foreach ($entry in $reserved) {
            if (Test-Path -LiteralPath $entry.FullName) {
                Remove-Item -LiteralPath $entry.FullName -Recurse -Force
            }
        }
    }
}

function Get-EditorExtensionRoot {
    param([string]$EditorName)

    switch ($EditorName.ToLowerInvariant()) {
        "cursor" { return Join-Path $env:USERPROFILE ".cursor\extensions" }
        "code" { return Join-Path $env:USERPROFILE ".vscode\extensions" }
        "code-insiders" { return Join-Path $env:USERPROFILE ".vscode-insiders\extensions" }
        "windsurf" { return Join-Path $env:USERPROFILE ".windsurf\extensions" }
        default { throw "Unsupported editor command: $EditorName" }
    }
}

function Get-EditorCli {
    param([string]$EditorName)

    $command = Get-Command $EditorName -ErrorAction SilentlyContinue
    if ($null -ne $command) {
        return $command.Source
    }
    $candidates = switch ($EditorName.ToLowerInvariant()) {
        "cursor" {
            "${env:LOCALAPPDATA}\Programs\cursor\resources\app\bin\cursor.cmd"
            "${env:LOCALAPPDATA}\Programs\cursor\Cursor.exe"
        }
        "code" {
            "${env:LOCALAPPDATA}\Programs\Microsoft VS Code\bin\code.cmd"
            "${env:ProgramFiles}\Microsoft VS Code\bin\code.cmd"
            "${env:ProgramFiles(x86)}\Microsoft VS Code\bin\code.cmd"
        }
        "code-insiders" {
            "${env:LOCALAPPDATA}\Programs\Microsoft VS Code Insiders\bin\code-insiders.cmd"
            "${env:ProgramFiles}\Microsoft VS Code Insiders\bin\code-insiders.cmd"
            "${env:ProgramFiles(x86)}\Microsoft VS Code Insiders\bin\code-insiders.cmd"
        }
        "windsurf" {
            "${env:LOCALAPPDATA}\Programs\Windsurf\bin\windsurf.cmd"
            "${env:LOCALAPPDATA}\Programs\Windsurf\Windsurf.exe"
        }
    }
    return $candidates | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
}

function Get-ReniumEditorInstalls {
    $installs = @()
    foreach ($editorName in @("cursor", "code", "code-insiders", "windsurf")) {
        $installs += [pscustomobject]@{
            Name = $editorName
            Cli = Get-EditorCli $editorName
            Root = Get-EditorExtensionRoot $editorName
        }
    }
    if (-not [string]::IsNullOrWhiteSpace($env:RENIUM_EXTENSION_ROOT)) {
        if ([string]::IsNullOrWhiteSpace($env:RENIUM_EDITOR_CLI)) {
            throw "RENIUM_EDITOR_CLI is required with RENIUM_EXTENSION_ROOT"
        }
        $customCli = Get-Command $env:RENIUM_EDITOR_CLI -ErrorAction Stop
        $customRoot = [IO.Path]::GetFullPath(
            [Environment]::ExpandEnvironmentVariables($env:RENIUM_EXTENSION_ROOT)
        )
        $existing = $installs | Where-Object {
            [string]::Equals(
                [IO.Path]::GetFullPath($_.Root),
                $customRoot,
                [StringComparison]::OrdinalIgnoreCase
            )
        } | Select-Object -First 1
        if ($null -eq $existing) {
            $installs += [pscustomobject]@{
                Name = [IO.Path]::GetFileNameWithoutExtension($customCli.Name)
                Cli = $customCli.Source
                Root = $customRoot
            }
        }
        elseif ($null -eq $existing.Cli) {
            $existing.Cli = $customCli.Source
        }
    }
    return $installs
}

function Get-ReniumEditorArchitecture {
    param([string]$Cli)
    $output = @(& $Cli --version 2>&1)
    if ($LASTEXITCODE -ne 0) {
        throw "Could not inspect the architecture of $Cli"
    }
    $architectures = @($output |
        ForEach-Object {
            [regex]::Matches(
                ([string]$_).ToLowerInvariant(),
                "(?<![a-z0-9_])(x64|x86_64|amd64|arm64|aarch64)(?![a-z0-9_])"
            ) | ForEach-Object {
                if ($_.Value -in @("x64", "x86_64", "amd64")) { "x64" } else { "arm64" }
            }
        } | Sort-Object -Unique)
    if ($architectures.Count -ne 1) {
        throw "$Cli did not report one supported architecture"
    }
    return $architectures[0]
}

function Get-EditorDisplayName {
    param([string]$Name)

    switch ($Name.ToLowerInvariant()) {
        "cursor" { return "Cursor" }
        "code" { return "Visual Studio Code" }
        "code-insiders" { return "Visual Studio Code Insiders" }
        "windsurf" { return "Windsurf" }
        default { return $Name }
    }
}

function Select-ReniumEditor {
    param([object[]]$Editors)

    Write-Host ""
    if ($Editors.Count -eq 0) {
        Write-Host "No supported editors were found. Install Cursor, Visual Studio Code, or Windsurf, then run this installer again."
        Write-Host "0. Exit"
        while ((Read-Host "Choose an option") -ne "0") {
            Write-Host "Enter 0 to exit."
        }
        return @()
    }
    Write-Host "Choose where to install the Renium extension:"
    for ($index = 0; $index -lt $Editors.Count; $index++) {
        Write-Host "$($index + 1). $(Get-EditorDisplayName $Editors[$index].Name)"
    }
    Write-Host "0. Exit"
    while ($true) {
        $choice = Read-Host "Choose an option"
        if ($choice -eq "0") {
            return @()
        }
        $number = 0
        if ([int]::TryParse($choice, [ref]$number) -and $number -ge 1 -and $number -le $Editors.Count) {
            return @($Editors[$number - 1])
        }
        Write-Host "Enter a number from 0 to $($Editors.Count)."
    }
}

function Write-ReniumTransactionJournal {
    param([object]$Journal, [string]$Path)
    $temporary = "$Path.$PID.tmp"
    $bytes = [Text.Encoding]::UTF8.GetBytes(($Journal | ConvertTo-Json -Depth 8))
    $stream = [IO.File]::Open(
        $temporary,
        [IO.FileMode]::Create,
        [IO.FileAccess]::Write,
        [IO.FileShare]::None
    )
    try {
        $stream.Write($bytes, 0, $bytes.Length)
        $stream.Flush($true)
    }
    finally {
        $stream.Dispose()
    }
    Move-Item -LiteralPath $temporary -Destination $Path
}

function Get-ReniumExtensionDirectories {
    param([string]$Root)
    if (-not (Test-Path -LiteralPath $Root -PathType Container)) {
        return @()
    }
    return @(Get-ChildItem -LiteralPath $Root -Directory | Where-Object {
        $_.Name -eq "local.renium" -or $_.Name.StartsWith("local.renium-", [StringComparison]::OrdinalIgnoreCase)
    })
}

function Start-ReniumInstallTransaction {
    param([array]$EditorInstalls)
    $root = Join-Path $env:LOCALAPPDATA "Renium\install-transaction"
    $journalPath = Join-Path $root "journal.json"
    if (Test-Path -LiteralPath $journalPath -PathType Leaf) {
        throw "An unfinished Renium install transaction must be recovered first"
    }
    if (Test-Path -LiteralPath $root) {
        Remove-Item -LiteralPath $root -Recurse -Force
    }
    New-Item -ItemType Directory -Path $root | Out-Null
    $coreBackup = Join-Path $root "core"
    $coreExisted = Test-Path -LiteralPath $installRoot -PathType Container
    if ($coreExisted) {
        Copy-Item -LiteralPath $installRoot -Destination $coreBackup -Recurse
    }
    $pluginPath = Join-Path $env:LOCALAPPDATA "Roblox\Plugins\Renium.rbxm"
    $pluginExisted = Test-Path -LiteralPath $pluginPath -PathType Leaf
    if ($pluginExisted) {
        Copy-Item -LiteralPath $pluginPath -Destination (Join-Path $root "plugin.rbxm")
    }
    $stableLauncherExisted = Test-Path -LiteralPath $stableLauncher -PathType Leaf
    if ($stableLauncherExisted) {
        Copy-Item -LiteralPath $stableLauncher -Destination (Join-Path $root "rbx.cmd")
    }
    $stableRunnerExisted = Test-Path -LiteralPath $stableRunner -PathType Leaf
    if ($stableRunnerExisted) {
        Copy-Item -LiteralPath $stableRunner -Destination (Join-Path $root "rbx-run.ps1")
    }
    $extensionSnapshots = @()
    $roots = @($EditorInstalls | ForEach-Object { [IO.Path]::GetFullPath($_.Root) } |
        Sort-Object -Unique)
    for ($index = 0; $index -lt $roots.Count; $index++) {
        $extensionRoot = $roots[$index]
        $backupRoot = Join-Path $root "extension-$index"
        New-Item -ItemType Directory -Path $backupRoot | Out-Null
        $existed = Test-Path -LiteralPath $extensionRoot -PathType Container
        if ($existed) {
            Get-ReniumExtensionDirectories $extensionRoot |
                ForEach-Object {
                    Copy-Item -LiteralPath $_.FullName -Destination $backupRoot -Recurse
                }
            $obsolete = Join-Path $extensionRoot ".obsolete"
            if (Test-Path -LiteralPath $obsolete -PathType Leaf) {
                Copy-Item -LiteralPath $obsolete -Destination (Join-Path $backupRoot ".obsolete")
            }
        }
        $extensionSnapshots += [pscustomobject]@{
            Root = $extensionRoot
            Backup = "extension-$index"
            Existed = $existed
        }
    }
    $journal = [ordered]@{
        InstallRoot = $installRoot
        CoreExisted = $coreExisted
        PluginPath = $pluginPath
        PluginExisted = $pluginExisted
        StableLauncher = $stableLauncher
        StableLauncherExisted = $stableLauncherExisted
        StableRunner = $stableRunner
        StableRunnerExisted = $stableRunnerExisted
        UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
        ExtensionSnapshots = $extensionSnapshots
    }
    Write-ReniumTransactionJournal $journal $journalPath
}

function Restore-ReniumInstallTransaction {
    $root = Join-Path $env:LOCALAPPDATA "Renium\install-transaction"
    $journalPath = Join-Path $root "journal.json"
    if (-not (Test-Path -LiteralPath $journalPath -PathType Leaf)) {
        if (Test-Path -LiteralPath $root) {
            Remove-Item -LiteralPath $root -Recurse -Force
        }
        return
    }
    $journal = Get-Content -LiteralPath $journalPath -Raw | ConvertFrom-Json
    foreach ($snapshot in @($journal.ExtensionSnapshots)) {
        New-Item -ItemType Directory -Path $snapshot.Root -Force | Out-Null
        Get-ReniumExtensionDirectories $snapshot.Root |
            Remove-Item -Recurse -Force
        $obsolete = Join-Path $snapshot.Root ".obsolete"
        if (Test-Path -LiteralPath $obsolete) {
            Remove-Item -LiteralPath $obsolete -Force
        }
        $backup = Join-Path $root $snapshot.Backup
        Get-ChildItem -LiteralPath $backup -Directory |
            ForEach-Object {
                Copy-Item -LiteralPath $_.FullName -Destination $snapshot.Root -Recurse
            }
        $obsoleteBackup = Join-Path $backup ".obsolete"
        if (Test-Path -LiteralPath $obsoleteBackup -PathType Leaf) {
            Copy-Item -LiteralPath $obsoleteBackup -Destination $obsolete
        }
        if (-not $snapshot.Existed -and
            (Get-ChildItem -LiteralPath $snapshot.Root -Force | Measure-Object).Count -eq 0) {
            Remove-Item -LiteralPath $snapshot.Root
        }
    }
    [Environment]::SetEnvironmentVariable("Path", [string]$journal.UserPath, "User")
    if (Test-Path -LiteralPath $journal.InstallRoot) {
        Remove-Item -LiteralPath $journal.InstallRoot -Recurse -Force
    }
    if ($journal.CoreExisted) {
        New-Item -ItemType Directory -Path (Split-Path -Parent $journal.InstallRoot) -Force |
            Out-Null
        Copy-Item -LiteralPath (Join-Path $root "core") -Destination $journal.InstallRoot -Recurse
    }
    if ($journal.CoreExisted) {
        Install-ReniumCommandAliases
    }
    else {
        @($stableExecutable, $stableReniumExecutable) |
            Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } |
            Remove-Item -Force
    }
    $transactionFiles = @(
        [pscustomobject]@{
            Path = [string]$journal.PluginPath
            Existed = [bool]$journal.PluginExisted
            Backup = "plugin.rbxm"
        },
        [pscustomobject]@{
            Path = [string]$journal.StableLauncher
            Existed = [bool]$journal.StableLauncherExisted
            Backup = "rbx.cmd"
        }
    )
    if ($journal.PSObject.Properties.Name -contains "StableRunner") {
        $transactionFiles += [pscustomobject]@{
            Path = [string]$journal.StableRunner
            Existed = [bool]$journal.StableRunnerExisted
            Backup = "rbx-run.ps1"
        }
    }
    foreach ($file in $transactionFiles) {
        if ($file.Existed) {
            New-Item -ItemType Directory -Path (Split-Path -Parent $file.Path) -Force | Out-Null
            Copy-Item -LiteralPath (Join-Path $root $file.Backup) -Destination $file.Path -Force
        }
        elseif (Test-Path -LiteralPath $file.Path -PathType Leaf) {
            Remove-Item -LiteralPath $file.Path -Force
        }
    }
    Remove-Item -LiteralPath $root -Recurse -Force
}

function Complete-ReniumInstallTransaction {
    $root = Join-Path $env:LOCALAPPDATA "Renium\install-transaction"
    $journalPath = Join-Path $root "journal.json"
    if (Test-Path -LiteralPath $journalPath -PathType Leaf) {
        Remove-Item -LiteralPath $journalPath -Force
    }
    if (Test-Path -LiteralPath $root) {
        Remove-Item -LiteralPath $root -Recurse -Force
    }
}

Enter-ReniumLifecycleLock
try {
    Restore-ReniumInstallTransaction
    Repair-ReniumCoreInstall
    $currentPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $pathEntries = @($currentPath -split ";" |
        Where-Object { -not [string]::IsNullOrWhiteSpace($_) })

    if ($Uninstall) {
    $editorInstalls = @(Get-ReniumEditorInstalls)
    $cli = Join-Path $installRoot "renium.exe"
    Stop-ReniumDaemons $cli $null
    Clear-ReniumUpdaterState
    Start-ReniumInstallTransaction $editorInstalls
    try {
    $extensionFailures = @()
    $pluginFailure = $false
    $coreFailure = $null
    $pathFailure = $null
    foreach ($editor in $editorInstalls) {
        if ($null -ne $editor.Cli) {
            try {
                & $editor.Cli --extensions-dir $editor.Root --uninstall-extension local.renium
                if ($LASTEXITCODE -ne 0) {
                    $extensionFailures += $editor.Name
                }
            }
            catch {
                $extensionFailures += $editor.Name
            }
        }
    }
    if (Test-Path -LiteralPath $cli -PathType Leaf) {
        try {
            & $cli setup --uninstall
            $pluginFailure = $LASTEXITCODE -ne 0
        }
        catch {
            $pluginFailure = $true
        }
    }
    foreach ($extensionRoot in @($editorInstalls | ForEach-Object { $_.Root } | Sort-Object -Unique)) {
        if (Test-Path -LiteralPath $extensionRoot -PathType Container) {
            try {
                Get-ReniumExtensionDirectories $extensionRoot |
                    Remove-Item -Recurse -Force
            }
            catch {
                $extensionFailures += $extensionRoot
            }
        }
    }
    $pluginPath = Join-Path $env:LOCALAPPDATA "Roblox\Plugins\Renium.rbxm"
    if (Test-Path -LiteralPath $pluginPath -PathType Leaf) {
        try {
            Remove-Item -LiteralPath $pluginPath -Force
        }
        catch {
            $pluginFailure = $true
        }
    }
    $resolvedInstall = [IO.Path]::GetFullPath($installRoot)
    try {
        $resolvedLocal = [IO.Path]::GetFullPath($env:LOCALAPPDATA).TrimEnd('\') + '\'
        if (-not $resolvedInstall.StartsWith($resolvedLocal, [StringComparison]::OrdinalIgnoreCase)) {
            throw "Refusing to remove an install directory outside LOCALAPPDATA"
        }
        if (Test-Path -LiteralPath $resolvedInstall) {
            Remove-Item -LiteralPath $resolvedInstall -Recurse -Force
        }
        if (Test-Path -LiteralPath $stableLauncher -PathType Leaf) {
            Remove-Item -LiteralPath $stableLauncher -Force
        }
        if (Test-Path -LiteralPath $stableRunner -PathType Leaf) {
            Remove-Item -LiteralPath $stableRunner -Force
        }
        if (Test-Path -LiteralPath $stableExecutable -PathType Leaf) {
            Remove-Item -LiteralPath $stableExecutable -Force
        }
        if (Test-Path -LiteralPath $stableReniumExecutable -PathType Leaf) {
            Remove-Item -LiteralPath $stableReniumExecutable -Force
        }
        if ((Test-Path -LiteralPath $stableLauncherRoot -PathType Container) -and
            @(Get-ChildItem -LiteralPath $stableLauncherRoot -Force).Count -eq 0) {
            Remove-Item -LiteralPath $stableLauncherRoot -Force
        }
    }
    catch {
        $coreFailure = $_.Exception.Message
    }
    try {
        $nextPath = ($pathEntries | Where-Object {
            $normalized = Get-NormalizedPathEntry $_
            -not [string]::Equals(
                $normalized,
                $resolvedInstall,
                [StringComparison]::OrdinalIgnoreCase
            )
        }) -join ";"
        [Environment]::SetEnvironmentVariable("Path", $nextPath, "User")
    }
    catch {
        $pathFailure = $_.Exception.Message
    }
    $partialFailures = @()
    if ($pluginFailure) {
        $partialFailures += "the Studio plugin"
    }
    if ($extensionFailures.Count -gt 0) {
        $partialFailures += "the extension in $($extensionFailures -join ', ')"
    }
    if ($null -ne $coreFailure) {
        $partialFailures += "the core ($coreFailure)"
    }
    if ($null -ne $pathFailure) {
        $partialFailures += "the user PATH ($pathFailure)"
    }
    if ($partialFailures.Count -gt 0) {
        $coreState = if ($coreFailure) { "the core may still be installed" } else { "the core was removed" }
        throw "Renium uninstall is incomplete; $coreState. Remaining work: $($partialFailures -join '; ')"
    }
    Complete-ReniumInstallTransaction
    Clear-ReniumUpdaterState
    Write-Host "Renium was uninstalled."
        return
    }
    catch {
        $originalError = $_
        try {
            Restore-ReniumInstallTransaction
        }
        catch {
            throw "$($originalError.Exception.Message) Rollback was incomplete: $($_.Exception.Message)"
        }
        throw $originalError
    }
    }

function Get-ReniumLocalVersion {
    param([string]$Cli)
    if (-not (Test-Path -LiteralPath $Cli -PathType Leaf)) {
        return $null
    }
    try {
        $output = @(& $Cli --version 2>&1)
        if ($LASTEXITCODE -eq 0 -and ($output -join " ") -match '\b(\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?)\b') {
            return $Matches[1]
        }
    }
    catch {
    }
    return $null
}

function Get-ReniumReleaseManifest {
    param(
        [string]$BaseUrl,
        [string]$Version,
        [string]$Destination
    )
    Invoke-WebRequest "$BaseUrl/update-manifest.json" -OutFile $Destination -UseBasicParsing
    $manifest = Get-Content -LiteralPath $Destination -Raw | ConvertFrom-Json
    if ($manifest.payload.schemaVersion -ne 1 -or [string]$manifest.payload.version -ne $Version) {
        throw "The Renium $Version update manifest is invalid"
    }
    return $manifest.payload
}

function Save-ReniumReleaseAsset {
    param(
        [object]$Manifest,
        [string]$Platform,
        [string]$Component,
        [string]$Name,
        [string]$BaseUrl,
        [string]$Destination
    )
    $platformProperty = $Manifest.components.PSObject.Properties[$Platform]
    $platformEntry = if ($null -ne $platformProperty) { $platformProperty.Value } else { $null }
    $asset = if ($null -ne $platformEntry) { $platformEntry.$Component } else { $null }
    $expectedUrl = "$BaseUrl/$Name"
    if ($null -eq $asset -or [string]$asset.url -ne $expectedUrl -or [string]$asset.sha256 -notmatch '^[0-9a-fA-F]{64}$') {
        throw "$Name is missing from the Renium update manifest"
    }
    Invoke-WebRequest $expectedUrl -OutFile $Destination -UseBasicParsing
    $actual = (Get-FileHash -LiteralPath $Destination -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne ([string]$asset.sha256).ToLowerInvariant()) {
        throw "$Name failed SHA-256 verification"
    }
}

$localCli = Join-Path $PSScriptRoot "renium.exe"
$localVersion = Get-ReniumLocalVersion $localCli
if ([string]::IsNullOrWhiteSpace($Version) -and $null -ne $localVersion) {
    $Version = $localVersion
}
if ([string]::IsNullOrWhiteSpace($Version)) {
    $release = Invoke-RestMethod "https://api.github.com/repos/$repository/releases/latest"
    $Version = ([string]$release.tag_name).TrimStart("v")
}

$architecture = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToLowerInvariant()
$releaseArchitecture = switch ($architecture) {
    "x64" { "x64" }
    "arm64" { "arm64" }
    default { throw "Renium does not provide a Windows build for $architecture" }
}
$archiveName = "renium-$Version-windows-$releaseArchitecture.zip"
$baseUrl = "https://github.com/$repository/releases/download/v$Version"
$editorInstalls = @(Get-ReniumEditorInstalls)
if (-not $Interactive) {
    foreach ($editor in $editorInstalls) {
        if ($null -ne $editor.Cli -or -not (Test-Path -LiteralPath $editor.Root -PathType Container)) {
            continue
        }
        $installedRenium = @(Get-ChildItem -LiteralPath $editor.Root -Directory -ErrorAction SilentlyContinue |
            Where-Object {
                $_.Name -eq "local.renium" -or $_.Name.StartsWith("local.renium-", [StringComparison]::OrdinalIgnoreCase)
            })
        if ($installedRenium.Count -gt 0) {
            throw "Renium is installed in $($editor.Root), but its exact editor CLI is unavailable. Set RENIUM_EXTENSION_ROOT to that path and RENIUM_EDITOR_CLI to the matching editor command."
        }
    }
}
$activeEditors = @($editorInstalls | Where-Object { $null -ne $_.Cli })
if ($Interactive) {
    $activeEditors = @(Select-ReniumEditor $activeEditors)
    if ($activeEditors.Count -eq 0) {
        Write-Host "Installation cancelled."
        exit 3
    }
}
foreach ($editor in $activeEditors) {
    $editor | Add-Member -NotePropertyName Architecture `
        -NotePropertyValue (Get-ReniumEditorArchitecture $editor.Cli) -Force
}
$useLocalPackage = $null -ne $localVersion -and $localVersion -eq $Version
$transactionId = [guid]::NewGuid().ToString("N")
$stage = Join-Path $env:TEMP "renium-install-$transactionId"
$installParent = Split-Path -Parent $installRoot
$stagedInstall = Join-Path $installParent (".renium-install-$transactionId")
$previousInstall = Join-Path $installParent (".renium-previous-$transactionId")

try {
    New-Item -ItemType Directory -Path $stage | Out-Null
    $releaseManifest = $null
    $manifestPath = Join-Path $stage "update-manifest.json"
    if ($useLocalPackage) {
        $cli = Get-Item -LiteralPath $localCli
    }
    else {
        $releaseManifest = Get-ReniumReleaseManifest $baseUrl $Version $manifestPath
        $archive = Join-Path $stage $archiveName
        $manifestPlatform = "windows-$(if ($releaseArchitecture -eq 'x64') { 'x86_64' } else { 'aarch64' })"
        Save-ReniumReleaseAsset $releaseManifest $manifestPlatform "cli" $archiveName $baseUrl $archive
        $expanded = Join-Path $stage "expanded"
        Expand-Archive -LiteralPath $archive -DestinationPath $expanded
        $cli = Get-ChildItem -LiteralPath $expanded -Recurse -File -Filter "renium.exe" | Select-Object -First 1
        if ($null -eq $cli) {
            throw "$archiveName does not contain renium.exe"
        }
    }
    $plugin = @(
        (Join-Path $PSScriptRoot "Renium.rbxm"),
        (Join-Path $cli.DirectoryName "Renium.rbxm")
    ) | Where-Object { Test-Path -LiteralPath $_ -PathType Leaf } | Select-Object -First 1
    if ($null -eq $plugin) {
        if ($null -eq $releaseManifest) {
            $releaseManifest = Get-ReniumReleaseManifest $baseUrl $Version $manifestPath
        }
        $plugin = Join-Path $stage "Renium.rbxm"
        $manifestPlatform = "windows-$(if ($releaseArchitecture -eq 'x64') { 'x86_64' } else { 'aarch64' })"
        Save-ReniumReleaseAsset $releaseManifest $manifestPlatform "plugin" "Renium.rbxm" $baseUrl $plugin
    }
    $vsixFiles = @{}
    foreach ($editorArchitecture in @($activeEditors.Architecture | Sort-Object -Unique)) {
        $editorTarget = "win32-$editorArchitecture"
        $vsixName = "renium-$Version-$editorTarget.vsix"
        $bundledVsix = Join-Path $PSScriptRoot $vsixName
        if (Test-Path -LiteralPath $bundledVsix -PathType Leaf) {
            $vsix = $bundledVsix
        }
        else {
            $vsix = Join-Path $stage $vsixName
            if ($null -eq $releaseManifest) {
                $releaseManifest = Get-ReniumReleaseManifest $baseUrl $Version $manifestPath
            }
            $manifestPlatform = "windows-$(if ($editorArchitecture -eq 'x64') { 'x86_64' } else { 'aarch64' })"
            Save-ReniumReleaseAsset $releaseManifest $manifestPlatform "extension" $vsixName $baseUrl $vsix
        }
        $vsixFiles[$editorArchitecture] = $vsix
    }
    New-Item -ItemType Directory -Path $installParent -Force | Out-Null
    if (Test-Path -LiteralPath $stagedInstall) {
        Remove-Item -LiteralPath $stagedInstall -Recurse -Force
    }
    if (Test-Path -LiteralPath $previousInstall) {
        Remove-Item -LiteralPath $previousInstall -Recurse -Force
    }
    New-Item -ItemType Directory -Path $stagedInstall | Out-Null
    Copy-Item -LiteralPath $cli.FullName -Destination (Join-Path $stagedInstall "renium.exe")
    foreach ($supportFile in @("rbx.cmd", "renium-agents.md")) {
        $supportPath = Join-Path $cli.DirectoryName $supportFile
        if (Test-Path -LiteralPath $supportPath -PathType Leaf) {
            Copy-Item -LiteralPath $supportPath -Destination $stagedInstall
        }
    }
    $guidePath = Join-Path $cli.DirectoryName "renium-guides"
    if (Test-Path -LiteralPath $guidePath -PathType Container) {
        Copy-Item -LiteralPath $guidePath -Destination $stagedInstall -Recurse
    }
    Copy-Item -LiteralPath $plugin -Destination (Join-Path $stagedInstall "Renium.rbxm")
    $existingCli = Join-Path $installRoot "renium.exe"
    $stagedCli = Join-Path $stagedInstall "renium.exe"
    Stop-ReniumDaemons $existingCli $stagedCli
    Clear-ReniumUpdaterState
    Start-ReniumInstallTransaction $activeEditors
    try {
        foreach ($editor in $activeEditors) {
            $vsix = $vsixFiles[$editor.Architecture]
            & $editor.Cli --extensions-dir $editor.Root --install-extension $vsix --force
            if ($LASTEXITCODE -ne 0) {
                throw "The editor extension installation failed in $($editor.Name) with exit code $LASTEXITCODE"
            }
        }
        if (Test-Path -LiteralPath $installRoot) {
            Move-Item -LiteralPath $installRoot -Destination $previousInstall
        }
        Move-Item -LiteralPath $stagedInstall -Destination $installRoot
        & (Join-Path $installRoot "renium.exe") setup
        if ($LASTEXITCODE -ne 0) {
            throw "The Studio plugin setup failed with exit code $LASTEXITCODE"
        }
        New-Item -ItemType Directory -Path $stableLauncherRoot -Force | Out-Null
        Copy-Item -LiteralPath (Join-Path $installRoot "rbx.cmd") -Destination $stableLauncher -Force
        if (Test-Path -LiteralPath $stableRunner -PathType Leaf) {
            Remove-Item -LiteralPath $stableRunner -Force
        }
        Install-ReniumCommandAliases
        if (-not ($pathEntries | Where-Object {
            $normalized = Get-NormalizedPathEntry $_
            [string]::Equals(
                $normalized,
                [IO.Path]::GetFullPath($installRoot),
                [StringComparison]::OrdinalIgnoreCase
            )
        })) {
            [Environment]::SetEnvironmentVariable(
                "Path",
                ((@($installRoot) + $pathEntries) -join ";"),
                "User"
            )
        }
    }
    catch {
        $originalError = $_
        try {
            Restore-ReniumInstallTransaction
        }
        catch {
            throw "$($originalError.Exception.Message) Rollback was incomplete: $($_.Exception.Message)"
        }
        throw $originalError
    }
    Complete-ReniumInstallTransaction
    Clear-ReniumUpdaterState
    if (Test-Path -LiteralPath $previousInstall) {
        try {
            Remove-Item -LiteralPath $previousInstall -Recurse -Force
        }
        catch {
            Write-Warning "Renium was installed, but the previous core could not be removed: $($_.Exception.Message)"
        }
    }
    Write-Host "Renium $Version was installed in $installRoot."
    Write-Host "Open a new terminal before using renium or rbx."
}
finally {
    if (Test-Path -LiteralPath $stagedInstall) {
        try {
            Remove-Item -LiteralPath $stagedInstall -Recurse -Force
        }
        catch {
            Write-Warning "Could not remove ${stagedInstall}: $($_.Exception.Message)"
        }
    }
    if (Test-Path -LiteralPath $stage) {
        try {
            Remove-Item -LiteralPath $stage -Recurse -Force
        }
        catch {
            Write-Warning "Could not remove ${stage}: $($_.Exception.Message)"
        }
    }
    }
}
finally {
    Exit-ReniumLifecycleLock
}
