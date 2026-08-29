$ErrorActionPreference = "Stop"

$pluginRoot = Split-Path -Parent $PSScriptRoot
$manifestPath = Join-Path $pluginRoot "Renium.project.json"
$configPath = Join-Path $pluginRoot "selene.toml"
$manifest = Get-Content -LiteralPath $manifestPath -Raw
$luaPaths = [regex]::Matches($manifest, '"\$path"\s*:\s*"(?<path>[^"]+\.lua)"') |
    ForEach-Object { Join-Path $pluginRoot $_.Groups["path"].Value } |
    Sort-Object -Unique

if ($luaPaths.Count -eq 0) {
    throw "Renium.project.json does not contain any shipped Luau files"
}

& selene --config $configPath @luaPaths
if ($LASTEXITCODE -ne 0) {
    throw "Shipped Renium plugin Luau failed validation"
}

Write-Output "All shipped Renium plugin Luau files pass validation"
