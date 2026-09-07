param([string]$Installer, [string]$Fixture)
$ErrorActionPreference = "Stop"

# Load only the two download functions. Never run the installer entry point.
$tokens = $null
$parseErrors = $null
$tree = [System.Management.Automation.Language.Parser]::ParseFile($Installer, [ref]$tokens, [ref]$parseErrors)
if ($parseErrors.Count -ne 0) { throw "Installer did not parse" }
foreach ($name in @('Get-ReniumReleaseManifest', 'Save-ReniumReleaseAsset')) {
    $function = $tree.Find({ param($node)
        $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name
    }, $true)
    if ($null -eq $function) { throw "Missing installer function: $name" }
    . ([scriptblock]::Create($function.Extent.Text))
}

$base = 'https://fixture.invalid/v0.3.4'
$assetName = 'renium.zip'
$source = Join-Path $Fixture 'fixture-asset'
$destination = Join-Path $Fixture 'downloaded-asset'
$manifestFile = Join-Path $Fixture 'downloaded-manifest.json'
$digest = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash
$script:manifest = @{ payload = @{ schemaVersion = 1; version = '0.3.4'; components = @{
    'windows-x64' = @{ cli = @{ url = "$base/$assetName"; sha256 = $digest } }
} } }
$script:downloads = 0
function Invoke-WebRequest {
    param([string]$Uri, [string]$OutFile, [switch]$UseBasicParsing)
    $script:downloads += 1
    if ($Uri -eq "$base/update-manifest.json") {
        $script:manifest | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $OutFile -Encoding utf8
    } elseif ($Uri -eq "$base/$assetName") {
        Copy-Item -LiteralPath $source -Destination $OutFile
    } else { throw "Unexpected download: $Uri" }
}
function Expect-Failure([scriptblock]$Action, [string]$Message) {
    try { & $Action } catch {
        if ($_.Exception.Message -notlike "*$Message*") { throw }
        return
    }
    throw "Expected failure: $Message"
}
function Read-Manifest {
    Get-ReniumReleaseManifest -BaseUrl $base -Version '0.3.4' -Destination $manifestFile
}
function Save-Asset($Payload) {
    Save-ReniumReleaseAsset -Manifest $Payload -Platform 'windows-x64' -Component 'cli' -Name $assetName -BaseUrl $base -Destination $destination
}

$payload = Read-Manifest
Save-Asset $payload
if ((Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash -ne $digest) { throw 'Wrong asset saved' }
$script:manifest.payload.version = 'wrong'
Expect-Failure { Read-Manifest } 'manifest is invalid'
$script:manifest.payload.version = '0.3.4'
$script:manifest.payload.schemaVersion = 2
Expect-Failure { Read-Manifest } 'manifest is invalid'
$script:manifest.payload.schemaVersion = 1
$payload.components.'windows-x64'.cli.sha256 = '0' * 64
Expect-Failure { Save-Asset $payload } 'SHA-256 verification'
$payload.components.'windows-x64'.cli.url = 'https://unexpected.invalid/renium.zip'
$before = $script:downloads
Expect-Failure { Save-Asset $payload } 'missing from the Renium update manifest'
if ($script:downloads -ne $before) { throw 'Invalid manifest triggered an asset download' }
Write-Output 'Installer manifest consumption and asset verification passed'
