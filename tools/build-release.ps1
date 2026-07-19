[CmdletBinding()]
param(
    [string]$OutputDirectory = "dist",
    [switch]$SkipTests,
    [switch]$AllowDirty,
    [switch]$AllowUnlicensed,
    [switch]$AllowLocalPublisher,
    [switch]$LocalBuild
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

if ($LocalBuild) {
    $AllowDirty = $true
    $AllowUnlicensed = $true
    $AllowLocalPublisher = $true
}

function Invoke-Checked {
    param(
        [Parameter(Mandatory = $true)][string]$File,
        [string[]]$Arguments = @(),
        [string]$WorkingDirectory
    )

    if ($WorkingDirectory) {
        Push-Location -LiteralPath $WorkingDirectory
    }
    try {
        Write-Host ("> {0} {1}" -f $File, ($Arguments -join " "))
        & $File @Arguments
        if ($LASTEXITCODE -ne 0) {
            throw "Command failed with exit code ${LASTEXITCODE}: $File $($Arguments -join ' ')"
        }
    }
    finally {
        if ($WorkingDirectory) {
            Pop-Location
        }
    }
}

function Invoke-CapturedChecked {
    param(
        [Parameter(Mandatory = $true)][string]$File,
        [string[]]$Arguments = @(),
        [string]$WorkingDirectory
    )

    if ($WorkingDirectory) {
        Push-Location -LiteralPath $WorkingDirectory
    }
    try {
        $output = & $File @Arguments 2>&1
        if ($LASTEXITCODE -ne 0) {
            throw "Command failed with exit code ${LASTEXITCODE}: $File $($Arguments -join ' ')`n$($output -join [Environment]::NewLine)"
        }
        return ($output -join [Environment]::NewLine).Trim()
    }
    finally {
        if ($WorkingDirectory) {
            Pop-Location
        }
    }
}

function Get-CargoPackageVersion {
    param([Parameter(Mandatory = $true)][string]$ManifestPath)

    $manifest = Get-Content -LiteralPath $ManifestPath -Raw
    $match = [regex]::Match($manifest, '(?m)^version\s*=\s*"(?<version>[^"]+)"\s*$')
    if (-not $match.Success) {
        throw "Could not read [package].version from $ManifestPath"
    }
    return $match.Groups["version"].Value
}

function Get-RepositoryRelativePath {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$RepositoryRoot
    )

    $relative = $Path.Substring($RepositoryRoot.Length).TrimStart([char[]]@('\', '/'))
    return $relative.Replace('\', '/')
}

function Get-ArtifactRecord {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][string]$ReleaseDirectory
    )

    $item = Get-Item -LiteralPath $Path
    $relative = $item.FullName.Substring($ReleaseDirectory.Length).TrimStart([char[]]@('\', '/')).Replace('\', '/')
    return [ordered]@{
        file = $relative
        bytes = $item.Length
        sha256 = (Get-FileHash -LiteralPath $item.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    }
}

$repositoryRoot = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot ".."))
$cargoManifest = Join-Path $repositoryRoot "tools\renium\Cargo.toml"
$cliDirectory = Split-Path -Parent $cargoManifest
$extensionDirectory = Join-Path $repositoryRoot "tools\renium-vscode-extension"
$extensionPackagePath = Join-Path $extensionDirectory "package.json"
$pluginDirectory = Join-Path $repositoryRoot "tools\plugin_ws_bridge"
$pluginProjectPath = Join-Path $pluginDirectory "Renium.project.json"
$pluginRuntimePath = Join-Path $pluginDirectory "BridgePluginRuntime.module.lua"

foreach ($requiredPath in @($cargoManifest, $extensionPackagePath, $pluginProjectPath, $pluginRuntimePath)) {
    if (-not (Test-Path -LiteralPath $requiredPath -PathType Leaf)) {
        throw "Required release input is missing: $requiredPath"
    }
}

foreach ($tool in @("git", "cargo", "node")) {
    if (-not (Get-Command -Name $tool -ErrorAction SilentlyContinue)) {
        throw "Required tool is not on PATH: $tool"
    }
}
if (-not $SkipTests -and -not (Get-Command -Name "lune" -ErrorAction SilentlyContinue)) {
    throw "Required tool is not on PATH: lune"
}

$npm = if ($env:OS -eq "Windows_NT") { "npm.cmd" } else { "npm" }
$npx = if ($env:OS -eq "Windows_NT") { "npx.cmd" } else { "npx" }
$rojo = if ([string]::IsNullOrWhiteSpace($env:RENIUM_ROJO)) { "rojo" } else { $env:RENIUM_ROJO }
foreach ($tool in @($npm, $npx, $rojo)) {
    if (-not (Get-Command -Name $tool -ErrorAction SilentlyContinue)) {
        throw "Required tool is not available: $tool. Install the version pinned in aftman.toml, or set RENIUM_ROJO for a custom Rojo path."
    }
}

$gitRoot = Invoke-CapturedChecked -File "git" -Arguments @("-C", $repositoryRoot, "rev-parse", "--show-toplevel")
if (-not [string]::Equals([IO.Path]::GetFullPath($gitRoot), $repositoryRoot, [StringComparison]::OrdinalIgnoreCase)) {
    throw "The build script must run from the repository containing $repositoryRoot"
}

$dirtyStatus = Invoke-CapturedChecked -File "git" -Arguments @("-C", $repositoryRoot, "status", "--porcelain", "--untracked-files=all")
if (-not $AllowDirty -and -not [string]::IsNullOrWhiteSpace($dirtyStatus)) {
    throw "Refusing a public release from a dirty checkout. Commit or stash the changes first, or use -LocalBuild for a private test artifact."
}
$revision = Invoke-CapturedChecked -File "git" -Arguments @("-C", $repositoryRoot, "rev-parse", "HEAD")

$cliVersion = Get-CargoPackageVersion -ManifestPath $cargoManifest
$extensionPackage = Get-Content -LiteralPath $extensionPackagePath -Raw | ConvertFrom-Json
$extensionVersion = [string]$extensionPackage.version
$pluginRuntime = Get-Content -LiteralPath $pluginRuntimePath -Raw
$pluginVersionMatch = [regex]::Match($pluginRuntime, '(?m)\bBRIDGE_VERSION\s*=\s*"(?<version>[^"]+)"')
if (-not $pluginVersionMatch.Success) {
    throw "Could not read BRIDGE_VERSION from $pluginRuntimePath"
}
$pluginVersion = $pluginVersionMatch.Groups["version"].Value

if (($cliVersion -ne $extensionVersion) -or ($cliVersion -ne $pluginVersion)) {
    throw "Version mismatch: CLI=$cliVersion, extension=$extensionVersion, plugin=$pluginVersion. All three must match before packaging."
}
$cliSourcePath = Join-Path $cliDirectory "src\main.rs"
$cliSource = Get-Content -LiteralPath $cliSourcePath -Raw
$compatibilityConstants = [ordered]@{
    BRIDGE_PROTOCOL_VERSION = "BRIDGE_PROTOCOL_VERSION"
    CODEC_VERSION = "BRIDGE_CODEC_VERSION_SCHEMA8"
    CHUNK_FRAME_PROTOCOL_VERSION = "BRIDGE_CHUNK_FRAME_PROTOCOL_VERSION"
    COMPACT_VALUE_PROTOCOL_VERSION = "BRIDGE_COMPACT_VALUE_PROTOCOL_VERSION"
}
foreach ($pluginConstant in $compatibilityConstants.Keys) {
    $pluginMatch = [regex]::Match($pluginRuntime, ('(?m)\b' + $pluginConstant + '\s*=\s*"(?<value>[^"]+)"'))
    $cliConstant = $compatibilityConstants[$pluginConstant]
    $cliMatch = [regex]::Match($cliSource, ('(?m)\b' + $cliConstant + '\s*:\s*&str\s*=\s*"(?<value>[^"]+)"'))
    if (-not $pluginMatch.Success -or -not $cliMatch.Success) {
        throw "Could not read compatibility metadata $pluginConstant/$cliConstant"
    }
    if ($pluginMatch.Groups["value"].Value -ne $cliMatch.Groups["value"].Value) {
        throw "Compatibility metadata mismatch: plugin $pluginConstant=$($pluginMatch.Groups["value"].Value), CLI $cliConstant=$($cliMatch.Groups["value"].Value)"
    }
}
if ($cliVersion -notmatch '^\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$') {
    throw "Unsafe release version '$cliVersion'"
}

$rootLicenses = @(Get-ChildItem -LiteralPath $repositoryRoot -File -Filter "LICENSE*" -ErrorAction SilentlyContinue)
if (-not $AllowUnlicensed -and $rootLicenses.Count -eq 0) {
    throw "A public release requires a root LICENSE file. Choose the product license first, or use -LocalBuild only for private testing."
}
if (-not $AllowLocalPublisher -and [string]::Equals([string]$extensionPackage.publisher, "local", [StringComparison]::OrdinalIgnoreCase)) {
    throw "The extension publisher is still 'local'. Configure a registered VS Code publisher before publishing, or use -LocalBuild only for an offline/private package."
}

$outputRoot = if ([IO.Path]::IsPathRooted($OutputDirectory)) {
    [IO.Path]::GetFullPath($OutputDirectory)
}
else {
    [IO.Path]::GetFullPath((Join-Path $repositoryRoot $OutputDirectory))
}
if (Test-Path -LiteralPath $outputRoot -PathType Leaf) {
    throw "Release output root is a file: $outputRoot"
}
New-Item -ItemType Directory -Path $outputRoot -Force | Out-Null

$releaseDirectory = [IO.Path]::GetFullPath((Join-Path $outputRoot ("renium-" + $cliVersion)))
$releasePrefix = $outputRoot.TrimEnd([char[]]@('\', '/')) + [IO.Path]::DirectorySeparatorChar
if (-not $releaseDirectory.StartsWith($releasePrefix, [StringComparison]::OrdinalIgnoreCase)) {
    throw "Refusing unsafe release output path: $releaseDirectory"
}
if (Test-Path -LiteralPath $releaseDirectory) {
    throw "Release output already exists: $releaseDirectory. Bump the version or choose a new -OutputDirectory; the script never overwrites an existing release."
}
New-Item -ItemType Directory -Path $releaseDirectory | Out-Null

$binaryName = if ($env:OS -eq "Windows_NT") { "renium.exe" } else { "renium" }
$cliBinary = Join-Path $cliDirectory ("target\release\" + $binaryName)

Invoke-Checked -File "cargo" -Arguments @("build", "--locked", "--release", "--manifest-path", $cargoManifest) -WorkingDirectory $repositoryRoot
if (-not $SkipTests) {
    Invoke-Checked -File "cargo" -Arguments @("test", "--locked", "--release", "--manifest-path", $cargoManifest) -WorkingDirectory $repositoryRoot
    Invoke-Checked -File "lune" -Arguments @("run", "tools/plugin_ws_bridge/tests/run") -WorkingDirectory $repositoryRoot
}
if (-not (Test-Path -LiteralPath $cliBinary -PathType Leaf)) {
    throw "Cargo reported success but did not create $cliBinary"
}
$releaseCliBinary = Join-Path $releaseDirectory $binaryName
Copy-Item -LiteralPath $cliBinary -Destination $releaseCliBinary
$releaseReadme = Join-Path $releaseDirectory "README.md"
$releaseCliReadme = Join-Path $releaseDirectory "CLI-README.md"
$releaseLicense = Join-Path $releaseDirectory "LICENSE"
Copy-Item -LiteralPath (Join-Path $repositoryRoot "README.md") -Destination $releaseReadme
Copy-Item -LiteralPath (Join-Path $repositoryRoot "tools\renium\README.md") -Destination $releaseCliReadme
Copy-Item -LiteralPath (Join-Path $repositoryRoot "LICENSE") -Destination $releaseLicense
$releaseSupportFiles = @($releaseReadme, $releaseCliReadme, $releaseLicense)
if ($env:OS -eq "Windows_NT") {
    $releaseRbx = Join-Path $releaseDirectory "rbx.cmd"
    $releaseRbxRunner = Join-Path $releaseDirectory "rbx-run.ps1"
    Copy-Item -LiteralPath (Join-Path $repositoryRoot "rbx.cmd") -Destination $releaseRbx
    Copy-Item -LiteralPath (Join-Path $repositoryRoot "tools\renium\rbx-run.ps1") -Destination $releaseRbxRunner
    $releaseSupportFiles += @($releaseRbx, $releaseRbxRunner)
}
else {
    $releaseRbx = Join-Path $releaseDirectory "rbx"
    Copy-Item -LiteralPath (Join-Path $repositoryRoot "rbx") -Destination $releaseRbx
    $releaseSupportFiles += $releaseRbx
}

$releasePluginXml = Join-Path $releaseDirectory "Renium.rbxmx"
$releasePluginBinary = Join-Path $releaseDirectory "Renium.rbxm"
Invoke-Checked -File $rojo -Arguments @("build", $pluginProjectPath, "--output", $releasePluginXml) -WorkingDirectory $repositoryRoot
Invoke-Checked -File $rojo -Arguments @("build", $pluginProjectPath, "--output", $releasePluginBinary) -WorkingDirectory $repositoryRoot

if ((Get-Item -LiteralPath $releasePluginXml).Length -lt 128 -or (Get-Item -LiteralPath $releasePluginBinary).Length -lt 16) {
    throw "Rojo produced an unexpectedly small plugin artifact"
}
if (-not ([IO.File]::ReadAllText($releasePluginXml).Contains("<roblox"))) {
    throw "The .rbxmx plugin artifact is not a Roblox XML model"
}

Copy-Item -LiteralPath $releasePluginXml -Destination (Join-Path $pluginDirectory "Renium.rbxmx") -Force
Copy-Item -LiteralPath $releasePluginBinary -Destination (Join-Path $pluginDirectory "Renium.rbxm") -Force
$extensionPluginBundle = Join-Path $extensionDirectory "assets\Renium.rbxm"
Copy-Item -LiteralPath $releasePluginBinary -Destination $extensionPluginBundle -Force

Invoke-Checked -File $npm -Arguments @("ci", "--prefix", $extensionDirectory) -WorkingDirectory $repositoryRoot
if (-not $SkipTests) {
    Invoke-Checked -File $npm -Arguments @("--prefix", $extensionDirectory, "run", "verify") -WorkingDirectory $repositoryRoot
}
$releaseVsix = Join-Path $releaseDirectory ("renium-" + $cliVersion + ".vsix")
Invoke-Checked -File $npx -Arguments @("--no-install", "vsce", "package", "--out", $releaseVsix) -WorkingDirectory $extensionDirectory
if (-not (Test-Path -LiteralPath $releaseVsix -PathType Leaf)) {
    throw "VSCE reported success but did not create $releaseVsix"
}

Add-Type -AssemblyName System.IO.Compression.FileSystem
$archive = [System.IO.Compression.ZipFile]::OpenRead($releaseVsix)
try {
    $entry = $archive.GetEntry("extension/package.json")
    if ($null -eq $entry) {
        throw "Packaged VSIX is missing extension/package.json"
    }
    $reader = New-Object System.IO.StreamReader($entry.Open())
    try {
        $packagedExtension = $reader.ReadToEnd() | ConvertFrom-Json
    }
    finally {
        $reader.Dispose()
    }
    $pluginEntry = $archive.GetEntry("extension/assets/Renium.rbxm")
    if ($null -eq $pluginEntry) {
        throw "Packaged VSIX is missing extension/assets/Renium.rbxm"
    }
    $pluginStream = $pluginEntry.Open()
    $sha256 = [Security.Cryptography.SHA256]::Create()
    try {
        $packagedPluginHash = [BitConverter]::ToString($sha256.ComputeHash($pluginStream)).Replace("-", "").ToLowerInvariant()
    }
    finally {
        $sha256.Dispose()
        $pluginStream.Dispose()
    }
}
finally {
    $archive.Dispose()
}
if (($packagedExtension.name -ne $extensionPackage.name) -or ($packagedExtension.version -ne $cliVersion) -or ($packagedExtension.publisher -ne $extensionPackage.publisher)) {
    throw "Packaged VSIX metadata does not match package.json"
}
$releasePluginHash = (Get-FileHash -LiteralPath $releasePluginBinary -Algorithm SHA256).Hash.ToLowerInvariant()
if ($packagedPluginHash -ne $releasePluginHash) {
    throw "Packaged VSIX plugin does not match the release plugin"
}

$pluginInputs = @(
    Get-ChildItem -LiteralPath $pluginDirectory -File -Recurse |
        Where-Object {
            $_.Name -notin @("Renium.rbxm", "Renium.rbxmx", ".renium-daemon.json")
        } |
        Sort-Object FullName |
        ForEach-Object {
            [ordered]@{
                file = Get-RepositoryRelativePath -Path $_.FullName -RepositoryRoot $repositoryRoot
                sha256 = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
            }
        }
)

$artifacts = @(
    Get-ArtifactRecord -Path $releaseCliBinary -ReleaseDirectory $releaseDirectory
    $releaseSupportFiles | ForEach-Object {
        Get-ArtifactRecord -Path $_ -ReleaseDirectory $releaseDirectory
    }
    Get-ArtifactRecord -Path $releaseVsix -ReleaseDirectory $releaseDirectory
    Get-ArtifactRecord -Path $releasePluginXml -ReleaseDirectory $releaseDirectory
    Get-ArtifactRecord -Path $releasePluginBinary -ReleaseDirectory $releaseDirectory
)

$manifest = [ordered]@{
    schemaVersion = 1
    product = "Renium"
    version = $cliVersion
    gitRevision = $revision
    dirtyCheckout = -not [string]::IsNullOrWhiteSpace($dirtyStatus)
    generatedAtUtc = [DateTime]::UtcNow.ToString("o")
    toolchain = [ordered]@{
        cargo = Invoke-CapturedChecked -File "cargo" -Arguments @("--version")
        node = Invoke-CapturedChecked -File "node" -Arguments @("--version")
        rojo = Invoke-CapturedChecked -File $rojo -Arguments @("--version") -WorkingDirectory $repositoryRoot
        vsce = Invoke-CapturedChecked -File $npx -Arguments @("--no-install", "vsce", "--version") -WorkingDirectory $extensionDirectory
    }
    inputs = [ordered]@{
        cargoLockSha256 = (Get-FileHash -LiteralPath (Join-Path $cliDirectory "Cargo.lock") -Algorithm SHA256).Hash.ToLowerInvariant()
        packageLockSha256 = (Get-FileHash -LiteralPath (Join-Path $extensionDirectory "package-lock.json") -Algorithm SHA256).Hash.ToLowerInvariant()
        pluginFiles = $pluginInputs
    }
    artifacts = $artifacts
}

$manifestPath = Join-Path $releaseDirectory "release-manifest.json"
$manifest | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $manifestPath -Encoding utf8

$checksumLines = $artifacts | ForEach-Object { "{0} *{1}" -f $_.sha256, $_.file }
$checksumLines | Set-Content -LiteralPath (Join-Path $releaseDirectory "SHA256SUMS.txt") -Encoding ascii

Write-Host "Release artifacts verified: $releaseDirectory"
if ($LocalBuild) {
    Write-Warning "This is a local/private build. It bypassed clean-checkout, license, and publisher guards and must not be published as a public release."
}
