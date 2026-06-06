param(
	[string]$Cli = $env:RENIUM_CLI,
	[string]$Code,
	[string]$File,
	[switch]$Client,
	[switch]$Play,
	[string]$Place = $env:RENIUM_PLACE,
	[int]$StudioWaitSeconds = 10,
	[int]$PlayWaitSeconds = 6,
	[int]$ConsoleLimit = 20,
	[int]$ConsoleWaitSeconds = 0,
	[switch]$NoConsole,
	[switch]$Raw
)

$ErrorActionPreference = "Stop"

function Resolve-ReniumCli {
	if ($Cli -and (Test-Path -LiteralPath $Cli)) {
		return (Resolve-Path -LiteralPath $Cli).Path
	}

	$fromPath = Get-Command renium.exe -ErrorAction SilentlyContinue
	if ($fromPath) {
		return $fromPath.Source
	}

	$root = Split-Path -Parent $MyInvocation.ScriptName
	$candidates = @(
		(Join-Path $root "renium.exe"),
		(Join-Path $root "bin\renium.exe"),
		(Join-Path $root "..\..\renium.exe"),
		(Join-Path $root "..\..\bin\renium.exe"),
		(Join-Path $root "target\release\renium.exe")
	)
	foreach ($candidate in $candidates) {
		if (Test-Path -LiteralPath $candidate) {
			return (Resolve-Path -LiteralPath $candidate).Path
		}
	}

	throw "Renium CLI not found. Put renium.exe on PATH, next to rbx.cmd, in bin\, or set RENIUM_CLI."
}

function Test-StudioRunning {
	return [bool](Get-Process -ErrorAction SilentlyContinue | Where-Object {
		$_.ProcessName -like "*RobloxStudio*" -or $_.ProcessName -like "*Roblox*Studio*"
	} | Select-Object -First 1)
}

function Start-ReniumDaemon {
	$existing = Get-Process renium -ErrorAction SilentlyContinue | Select-Object -First 1
	if ($existing) {
		return
	}
	Start-Process -FilePath $script:CliPath -ArgumentList @("bd", "-s") -WindowStyle Hidden -WorkingDirectory (Get-Location).Path
	Start-Sleep -Milliseconds 500
}

function Ensure-Studio {
	if (Test-StudioRunning) {
		return
	}
	if (-not $Place) {
		throw "Studio is not running. Start Studio first, pass -Place, or set RENIUM_PLACE."
	}
	if (-not (Test-Path -LiteralPath $Place)) {
		throw "Place file not found: $Place"
	}
	Start-Process -FilePath $Place
	Start-Sleep -Seconds $StudioWaitSeconds
}

function Invoke-Renium {
	param([string[]]$CommandArgs)
	$oldErrorActionPreference = $ErrorActionPreference
	$ErrorActionPreference = "Continue"
	try {
		$output = & $script:CliPath @CommandArgs 2>&1
		$code = $LASTEXITCODE
	} finally {
		$ErrorActionPreference = $oldErrorActionPreference
	}
	[pscustomobject]@{
		Code = $code
		Text = ($output | ForEach-Object { $_.ToString() } | Out-String).Trim()
	}
}

function Wait-ReniumReady {
	$deadline = [DateTime]::UtcNow.AddSeconds($StudioWaitSeconds)
	do {
		$result = Invoke-Renium @("lx", "-e", "return true")
		if ($result.Code -eq 0) {
			return
		}
		Start-Sleep -Milliseconds 500
	} while ([DateTime]::UtcNow -lt $deadline)
	throw "Renium bridge is not ready: $($result.Text)"
}

function Start-PlayIfNeeded {
	if (-not $Client -and -not $Play) {
		return
	}
	$result = Invoke-Renium @("play", "-s")
	if ($result.Code -ne 0 -and $result.Text -notmatch "Timed out waiting for Studio test session to start") {
		throw $result.Text
	}
	Start-Sleep -Seconds $PlayWaitSeconds
}

function Get-ConsoleSeq {
	$result = Invoke-Renium @("co", "-n", "1")
	if ($result.Code -ne 0 -or -not $result.Text) {
		return 0
	}
	try {
		$json = $result.Text | ConvertFrom-Json
		if ($null -ne $json.nextSeq) {
			return [uint64]$json.nextSeq
		}
	} catch {
		return 0
	}
	return 0
}

function Invoke-Luau {
	$args = @("lx")
	if ($Client) {
		$args += "-c"
	}
	if ($File) {
		$args += @("-f", $File)
	} else {
		$args += @("-e", $Code)
	}

	$result = Invoke-Renium $args
	if ($result.Code -ne 0) {
		throw $result.Text
	}
	if ($Raw -and $result.Text) {
		Write-Output $result.Text
	}
}

function Write-RecentConsole {
	if ($NoConsole) {
		return
	}
	$deadline = [DateTime]::UtcNow.AddSeconds([Math]::Max(0.5, $ConsoleWaitSeconds))
	$printed = New-Object 'System.Collections.Generic.HashSet[uint64]'
	do {
		Start-Sleep -Milliseconds 250
		$jsonText = (Invoke-Renium @("co", "-n", [string]$ConsoleLimit, "-s", [string]$script:ConsoleSinceSeq)).Text
		if ($jsonText) {
			$json = $jsonText | ConvertFrom-Json
			foreach ($entry in $json.entries) {
				$seq = [uint64]$entry.seq
				if ($printed.Add($seq)) {
					$msg = [string]$entry.message -replace "`r?`n", " | "
					"{0}: {1}" -f $entry.type, $msg
				}
			}
		}
	} while ($ConsoleWaitSeconds -gt 0 -and [DateTime]::UtcNow -lt $deadline)
}

if (-not $Code -and -not $File) {
	throw "Missing Luau. Pass -Code or -File."
}

$script:CliPath = Resolve-ReniumCli
Start-ReniumDaemon
Ensure-Studio
Wait-ReniumReady
Start-PlayIfNeeded
$script:ConsoleSinceSeq = Get-ConsoleSeq
Invoke-Luau
Write-RecentConsole
