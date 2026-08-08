param(
	[string]$Cli = $env:RENIUM_CLI,
	[string]$Code,
	[string]$File,
	[switch]$Client,
	[string]$Player,
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

	$fromPath = Get-Command renium.exe -ErrorAction SilentlyContinue
	if ($fromPath) {
		return $fromPath.Source
	}

	throw "Renium CLI not found. Put renium.exe on PATH, next to rbx.cmd, in bin\, or set RENIUM_CLI."
}

function Test-StudioRunning {
	return [bool](Get-Process -ErrorAction SilentlyContinue | Where-Object {
		$_.ProcessName -like "*RobloxStudio*" -or $_.ProcessName -like "*Roblox*Studio*"
	} | Select-Object -First 1)
}

function Start-ReniumDaemon {
	$status = Invoke-Renium @("daemon", "status")
	if ($status.Code -eq 0) {
		return
	}
	Start-Process -FilePath $script:CliPath -ArgumentList @("bd", "-s") -WindowStyle Hidden -WorkingDirectory (Get-Location).Path
	$deadline = [DateTime]::UtcNow.AddSeconds(5)
	do {
		Start-Sleep -Milliseconds 100
		$status = Invoke-Renium @("daemon", "status")
		if ($status.Code -eq 0) {
			return
		}
	} while ([DateTime]::UtcNow -lt $deadline)
	throw "Renium daemon did not become ready: $($status.Text)"
}

function Ensure-Studio {
	if (-not $Place -and (Test-StudioRunning)) {
		return
	}
	if (-not $Place) {
		throw "Studio is not running. Start Studio first, pass -Place, or set RENIUM_PLACE."
	}
	$ready = Invoke-Renium @("lx", "-e", "return true")
	if ($ready.Code -eq 0) {
		return
	}
	if (-not (Test-Path -LiteralPath $Place)) {
		if (Test-StudioRunning) {
			throw "The requested Studio place is not connected: $Place"
		}
		throw "Place file not found: $Place"
	}
	Start-Process -FilePath $Place
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
	if (-not $Client -and -not $Player -and -not $Play) {
		return
	}
	if ($Client -or $Player) {
		$probeArgs = @("lx", "-e", "return true")
		if ($Player) {
			$probeArgs += @("--player", $Player)
		} else {
			$probeArgs += "-c"
		}
		if ((Invoke-Renium $probeArgs).Code -eq 0) {
			return
		}
	}
	$result = Invoke-Renium @("play", "-s")
	if ($result.Code -ne 0) {
		throw $result.Text
	}
	$deadline = [DateTime]::UtcNow.AddSeconds($PlayWaitSeconds)
	do {
		if ($Client -or $Player) {
			$probeArgs = @("lx", "-e", "return true")
			if ($Player) {
				$probeArgs += @("--player", $Player)
			} else {
				$probeArgs += "-c"
			}
			if ((Invoke-Renium $probeArgs).Code -eq 0) {
				return
			}
		} else {
			$clients = Invoke-Renium @("clients")
			if ($clients.Code -eq 0) {
				try {
					$parsed = $clients.Text | ConvertFrom-Json
					if (@($parsed.clients | Where-Object { $_.role -eq "play-server" }).Count -gt 0) {
						return
					}
				} catch {
				}
			}
		}
		Start-Sleep -Milliseconds 100
	} while ([DateTime]::UtcNow -lt $deadline)
	if ($Client -or $Player) {
		throw "A Studio play client did not connect."
	}
	throw "The Studio play server did not become ready."
}

function Get-ConsoleArgs {
	param([int]$Limit, [uint64]$Since)
	$args = @("co", "-n", [string]$Limit, "-s", [string]$Since)
	if ($Player) {
		$args += @("--player", $Player)
	} elseif ($Client) {
		$args += "--client"
	}
	return $args
}

function Get-ConsoleState {
	$result = Invoke-Renium (Get-ConsoleArgs -Limit 1 -Since 0)
	if ($result.Code -ne 0 -or -not $result.Text) {
		throw "Could not read the Studio console baseline: $($result.Text)"
	}
	try {
		$json = $result.Text | ConvertFrom-Json
		if ($null -ne $json.nextSeq -and $json.epoch) {
			return [pscustomobject]@{
				Seq = [uint64]$json.nextSeq
				Epoch = [string]$json.epoch
			}
		}
	} catch {
		throw "Studio console baseline was malformed: $($_.Exception.Message)"
	}
	throw "Studio console baseline did not include an epoch and sequence."
}

function Invoke-Luau {
	$args = @("lx")
	if ($Player) {
		$args += @("--player", $Player)
	} elseif ($Client) {
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
	$cursor = [uint64]$script:ConsoleSinceSeq
	$epoch = [string]$script:ConsoleEpoch
	do {
		Start-Sleep -Milliseconds 250
		while ($true) {
			$page = Invoke-Renium (Get-ConsoleArgs -Limit $ConsoleLimit -Since $cursor)
			if ($page.Code -ne 0) {
				throw "Could not read the Studio console: $($page.Text)"
			}
			$jsonText = $page.Text
			if (-not $jsonText) {
				break
			}
			$json = $jsonText | ConvertFrom-Json
			if ([string]$json.epoch -ne $epoch) {
				$epoch = [string]$json.epoch
				$cursor = 0
				$printed.Clear()
				$script:ConsoleEpoch = $epoch
				continue
			}
			if ($json.truncated) {
				throw "Studio console output was truncated before it could be read."
			}
			foreach ($entry in $json.entries) {
				$seq = [uint64]$entry.seq
				if ($printed.Add($seq)) {
					$msg = [string]$entry.message -replace "`r?`n", " | "
					"{0}: {1}" -f $entry.type, $msg
				}
			}
			$next = [uint64]$json.nextSeq
			if ($json.hasMore -and $next -le $cursor) {
				throw "Renium console cursor did not advance."
			}
			$cursor = $next
			$script:ConsoleSinceSeq = $cursor
			if (-not $json.hasMore) {
				break
			}
		}
	} while ($ConsoleWaitSeconds -gt 0 -and [DateTime]::UtcNow -lt $deadline)
}

if (-not $Code -and -not $File) {
	throw "Missing Luau. Pass -Code or -File."
}

$script:CliPath = Resolve-ReniumCli
if ($Place) {
	$placeSelector = if (Test-Path -LiteralPath $Place) {
		[System.IO.Path]::GetFileNameWithoutExtension((Resolve-Path -LiteralPath $Place).Path)
	} else {
		$Place
	}
	$env:RENIUM_PLACE = $placeSelector
}
Start-ReniumDaemon
Ensure-Studio
Wait-ReniumReady
Start-PlayIfNeeded
$consoleState = Get-ConsoleState
$script:ConsoleSinceSeq = $consoleState.Seq
$script:ConsoleEpoch = $consoleState.Epoch
Invoke-Luau
Write-RecentConsole
