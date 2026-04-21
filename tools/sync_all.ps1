param(
    [string]$ProjectRoot = (Resolve-Path ".").Path,
    [string]$SnapshotDir = "snapshots",
    [int]$SourceWorkers = 0
)

$ErrorActionPreference = "Stop"

$projectRootResolved = (Resolve-Path $ProjectRoot).Path
$snapshotDirAbs = Join-Path $projectRootResolved $SnapshotDir

Push-Location $projectRootResolved
try {
    $exportArgs = @(
        ".\tools\mcp_sync_export.py",
        "--project-root", ".",
        "--snapshot-dir", $SnapshotDir
    )
    if ($SourceWorkers -gt 0) {
        $exportArgs += @("--source-workers", "$SourceWorkers")
    }

    Write-Output "[sync-all] exporting snapshots..."
    & python @exportArgs
    if ($LASTEXITCODE -ne 0) {
        throw "Export failed with exit code $LASTEXITCODE"
    }

    Write-Output "[sync-all] importing into src..."
    & powershell -ExecutionPolicy Bypass -File ".\tools\import_snapshot.ps1" -SnapshotDir $snapshotDirAbs -ProjectRoot $projectRootResolved
    if ($LASTEXITCODE -ne 0) {
        throw "Import failed with exit code $LASTEXITCODE"
    }

    $workspacePath = Join-Path $projectRootResolved "src\Workspace"
    if (-not (Test-Path -LiteralPath $workspacePath)) {
        throw "Sync finished but src\\Workspace is missing."
    }

    Write-Output "[sync-all] done"
}
finally {
    Pop-Location
}

