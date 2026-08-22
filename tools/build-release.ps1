[CmdletBinding()]
param(
    [string]$OutputDirectory = "dist",
    [switch]$SkipTests,
    [switch]$AllowDirty,
    [switch]$AllowUnlicensed,
    [switch]$AllowLocalPublisher,
    [switch]$LocalBuild,
    [string]$TargetTriple,
    [string]$PrebuiltCli
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

function Get-ReleaseTarget {
    param([string]$Requested)

    if (-not [string]::IsNullOrWhiteSpace($Requested)) {
        $triple = $Requested.Trim()
    }
    else {
        $architecture = [Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString().ToLowerInvariant()
        $cpu = switch ($architecture) {
            "x64" { "x86_64" }
            "arm64" { "aarch64" }
            default { throw "Renium does not provide a release build for $architecture" }
        }
        $os = if ($env:OS -eq "Windows_NT") {
            "pc-windows-msvc"
        }
        elseif ($IsMacOS) {
            "apple-darwin"
        }
        else {
            "unknown-linux-gnu"
        }
        $triple = "$cpu-$os"
    }

    $match = [regex]::Match(
        $triple,
        '^(?<cpu>x86_64|aarch64)-(?<os>pc-windows-msvc|apple-darwin|unknown-linux-gnu)$'
    )
    if (-not $match.Success) {
        throw "Unsupported Renium release target '$triple'"
    }
    $platform = switch ($match.Groups["os"].Value) {
        "pc-windows-msvc" { "win32" }
        "apple-darwin" { "darwin" }
        "unknown-linux-gnu" { "linux" }
    }
    $architecture = if ($match.Groups["cpu"].Value -eq "x86_64") { "x64" } else { "arm64" }
    return [pscustomobject]@{
        Triple = $triple
        Platform = $platform
        Architecture = $architecture
    }
}

function Get-BinaryArchitecture {
    param([Parameter(Mandatory = $true)][string]$Path)

    [byte[]]$bytes = [IO.File]::ReadAllBytes($Path)
    if ($bytes.Length -lt 20) {
        throw "Release executable is too small to identify: $Path"
    }
    if ($bytes[0] -eq 0x4D -and $bytes[1] -eq 0x5A) {
        $header = [BitConverter]::ToInt32($bytes, 0x3C)
        if ($header -lt 0 -or $header + 6 -gt $bytes.Length) {
            throw "Release executable has an invalid PE header: $Path"
        }
        $machine = [BitConverter]::ToUInt16($bytes, $header + 4)
        $architecture = switch ($machine) {
            0x8664 { "x64" }
            0xAA64 { "arm64" }
            default { throw "Unsupported PE machine 0x$($machine.ToString('X4')) in $Path" }
        }
        return $architecture
    }
    if ($bytes[0] -eq 0x7F -and $bytes[1] -eq 0x45 -and $bytes[2] -eq 0x4C -and $bytes[3] -eq 0x46) {
        $machine = if ($bytes[5] -eq 1) {
            [BitConverter]::ToUInt16($bytes, 18)
        }
        else {
            [uint16](($bytes[18] -shl 8) -bor $bytes[19])
        }
        $architecture = switch ($machine) {
            62 { "x64" }
            183 { "arm64" }
            default { throw "Unsupported ELF machine $machine in $Path" }
        }
        return $architecture
    }
    $magic = [BitConverter]::ToUInt32($bytes, 0)
    if ($magic -in @(0xFEEDFACF, 0xCFFAEDFE)) {
        $cpu = if ($magic -eq 0xFEEDFACF) {
            [BitConverter]::ToUInt32($bytes, 4)
        }
        else {
            [uint32](($bytes[4] -shl 24) -bor ($bytes[5] -shl 16) -bor ($bytes[6] -shl 8) -bor $bytes[7])
        }
        $architecture = switch ($cpu) {
            0x01000007 { "x64" }
            0x0100000C { "arm64" }
            default { throw "Unsupported Mach-O CPU 0x$($cpu.ToString('X8')) in $Path" }
        }
        return $architecture
    }
    throw "Unsupported executable format: $Path"
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

function Copy-ExtensionStage {
    param(
        [Parameter(Mandatory = $true)][string]$Source,
        [Parameter(Mandatory = $true)][string]$Destination
    )

    New-Item -ItemType Directory -Path $Destination | Out-Null
    foreach ($entry in Get-ChildItem -LiteralPath $Source -Force) {
        if ($entry.Name -in @("node_modules", "out", "out-test", "bin")) {
            continue
        }
        Copy-Item -LiteralPath $entry.FullName -Destination $Destination -Recurse -Force
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
if ([string]::IsNullOrWhiteSpace($env:SOURCE_DATE_EPOCH)) {
    $env:SOURCE_DATE_EPOCH = Invoke-CapturedChecked -File "git" -Arguments @(
        "-C",
        $repositoryRoot,
        "show",
        "-s",
        "--format=%ct",
        "HEAD"
    )
}
$sourceDateEpoch = 0L
if (-not [long]::TryParse($env:SOURCE_DATE_EPOCH, [ref]$sourceDateEpoch) -or $sourceDateEpoch -lt 0) {
    throw "SOURCE_DATE_EPOCH must be a non-negative Unix timestamp"
}
$generatedAtUtc = [DateTimeOffset]::FromUnixTimeSeconds($sourceDateEpoch).UtcDateTime.ToString("o")

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
$cliProtocolSourcePath = Join-Path $cliDirectory "src\snapshot\export.rs"
$cliProtocolSource = Get-Content -LiteralPath $cliProtocolSourcePath -Raw
$compatibilityConstants = [ordered]@{
    BRIDGE_PROTOCOL_VERSION = "BRIDGE_PROTOCOL_VERSION"
    CHUNK_FRAME_PROTOCOL_VERSION = "BRIDGE_CHUNK_FRAME_PROTOCOL_VERSION"
    COMPACT_VALUE_PROTOCOL_VERSION = "BRIDGE_COMPACT_VALUE_PROTOCOL_VERSION"
}
foreach ($pluginConstant in $compatibilityConstants.Keys) {
    $pluginMatch = [regex]::Match($pluginRuntime, ('(?m)\b' + $pluginConstant + '\s*=\s*"(?<value>[^"]+)"'))
    $cliConstant = $compatibilityConstants[$pluginConstant]
    $cliMatch = [regex]::Match($cliProtocolSource, ('(?m)\b' + $cliConstant + '\s*:\s*&str\s*=\s*"(?<value>[^"]+)"'))
    if (-not $pluginMatch.Success -or -not $cliMatch.Success) {
        throw "Could not read compatibility metadata $pluginConstant/$cliConstant"
    }
    if ($pluginMatch.Groups["value"].Value -ne $cliMatch.Groups["value"].Value) {
        throw "Compatibility metadata mismatch: plugin $pluginConstant=$($pluginMatch.Groups["value"].Value), CLI $cliConstant=$($cliMatch.Groups["value"].Value)"
    }
}
$pluginCodecMatch = [regex]::Match(
    $pluginRuntime,
    '(?ms)\bCODEC_VERSION\s*=\s*if\b.*?\bthen\s*"(?<primary>[^"]+)"\s*\belse\s*"(?<fallback>[^"]+)"'
)
$cliPrimaryCodecMatch = [regex]::Match(
    $cliProtocolSource,
    '(?m)\bBRIDGE_CODEC_VERSION_SCHEMA9\s*:\s*&str\s*=\s*"(?<value>[^"]+)"'
)
$cliFallbackCodecMatch = [regex]::Match(
    $cliProtocolSource,
    '(?m)\bBRIDGE_CODEC_VERSION_SCHEMA8\s*:\s*&str\s*=\s*"(?<value>[^"]+)"'
)
if (-not $pluginCodecMatch.Success -or -not $cliPrimaryCodecMatch.Success -or -not $cliFallbackCodecMatch.Success) {
    throw "Could not read conditional codec compatibility metadata"
}
if (
    $pluginCodecMatch.Groups["primary"].Value -ne $cliPrimaryCodecMatch.Groups["value"].Value -or
    $pluginCodecMatch.Groups["fallback"].Value -ne $cliFallbackCodecMatch.Groups["value"].Value
) {
    throw "Conditional codec compatibility metadata does not match the CLI"
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
if (-not $LocalBuild) {
    $updatePublicKey = [string]$env:RENIUM_UPDATE_PUBLIC_KEY
    if ([string]::IsNullOrWhiteSpace($updatePublicKey)) {
        throw "A public release requires RENIUM_UPDATE_PUBLIC_KEY."
    }
    try {
        $updatePublicKeyBytes = [Convert]::FromBase64String($updatePublicKey.Trim())
    }
    catch {
        throw "RENIUM_UPDATE_PUBLIC_KEY must be valid base64."
    }
    if ($updatePublicKeyBytes.Length -ne 32) {
        throw "RENIUM_UPDATE_PUBLIC_KEY must decode to 32 bytes."
    }
    $env:RENIUM_UPDATE_PUBLIC_KEY = $updatePublicKey.Trim()
}
$releaseTarget = Get-ReleaseTarget -Requested $TargetTriple
$hostPlatform = if ($env:OS -eq "Windows_NT") {
    "win32"
}
elseif ($IsMacOS) {
    "darwin"
}
else {
    "linux"
}
if ($releaseTarget.Platform -ne $hostPlatform) {
    throw "Release target $($releaseTarget.Triple) does not match the current operating system."
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

$finalReleaseDirectory = [IO.Path]::GetFullPath((Join-Path $outputRoot ("renium-" + $cliVersion)))
$releasePrefix = $outputRoot.TrimEnd([char[]]@('\', '/')) + [IO.Path]::DirectorySeparatorChar
if (-not $finalReleaseDirectory.StartsWith($releasePrefix, [StringComparison]::OrdinalIgnoreCase)) {
    throw "Refusing unsafe release output path: $finalReleaseDirectory"
}
if (Test-Path -LiteralPath $finalReleaseDirectory) {
    throw "Release output already exists: $finalReleaseDirectory. Bump the version or choose a new -OutputDirectory; the script never overwrites an existing release."
}
$releaseDirectory = Join-Path $outputRoot (".renium-" + $cliVersion + ".stage-" + [guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Path $releaseDirectory | Out-Null

$previousPluginBundle = $env:RENIUM_PLUGIN_BUNDLE
$previousCliBuild = $env:RENIUM_CLI_BUILD
$previousIconPath = $env:RENIUM_INSERTABLE_OBJECTS_ICON_PATH
$previousSkipPluginSchema = $env:RENIUM_SKIP_PLUGIN_SCHEMA_WRITE
$previousTargetPlatform = $env:RENIUM_CLI_TARGET_PLATFORM
$previousTargetArchitecture = $env:RENIUM_CLI_TARGET_ARCH
$extensionStage = Join-Path (Split-Path -Parent $extensionDirectory) (".renium-extension-stage-" + [guid]::NewGuid().ToString("N"))
$buildError = $null
$restoreErrors = @()
try {
$binaryName = if ($env:OS -eq "Windows_NT") { "renium.exe" } else { "renium" }
$cliBinary = if ([string]::IsNullOrWhiteSpace($PrebuiltCli)) {
    Join-Path $cliDirectory ("target\" + $releaseTarget.Triple + "\release\" + $binaryName)
}
else {
    [IO.Path]::GetFullPath($PrebuiltCli)
}

if ([string]::IsNullOrWhiteSpace($PrebuiltCli)) {
    Invoke-Checked -File "cargo" -Arguments @("build", "--locked", "--release", "--target", $releaseTarget.Triple, "--manifest-path", $cargoManifest) -WorkingDirectory $repositoryRoot
}
if (-not $SkipTests) {
    if ([string]::IsNullOrWhiteSpace($PrebuiltCli)) {
        Invoke-Checked -File "cargo" -Arguments @("test", "--locked", "--release", "--target", $releaseTarget.Triple, "--manifest-path", $cargoManifest) -WorkingDirectory $repositoryRoot
    }
    Invoke-Checked -File "lune" -Arguments @("run", "tools/plugin_ws_bridge/tests/run") -WorkingDirectory $repositoryRoot
    if ($env:OS -eq "Windows_NT") {
        Invoke-Checked -File "node" -Arguments @("tools/renium/tests/automation-replay.mjs", $cliBinary) -WorkingDirectory $repositoryRoot
        Invoke-Checked -File "node" -Arguments @("tools/renium/tests/agent-docs-smoke.mjs", $cliBinary) -WorkingDirectory $repositoryRoot
        Invoke-Checked -File "node" -Arguments @("tools/renium/tests/launcher-smoke.mjs") -WorkingDirectory $repositoryRoot
    }
}
if (-not (Test-Path -LiteralPath $cliBinary -PathType Leaf)) {
    throw "Release CLI does not exist: $cliBinary"
}
if ((Get-BinaryArchitecture -Path $cliBinary) -ne $releaseTarget.Architecture) {
    throw "Cargo built a CLI whose architecture does not match $($releaseTarget.Triple)"
}
$releaseCliBinary = Join-Path $releaseDirectory $binaryName
Copy-Item -LiteralPath $cliBinary -Destination $releaseCliBinary
$releaseReadme = Join-Path $releaseDirectory "README.md"
$releaseLicense = Join-Path $releaseDirectory "LICENSE"
$releaseAgentInstructions = Join-Path $releaseDirectory "renium-agents.md"
$releaseAgentGuides = Join-Path $releaseDirectory "renium-guides"
$releaseReadmeText = [IO.File]::ReadAllText(
    (Join-Path $repositoryRoot "tools\renium\RELEASE_README.md")
).Replace("{{VERSION}}", $cliVersion)
[IO.File]::WriteAllText($releaseReadme, $releaseReadmeText, [Text.UTF8Encoding]::new($false))
Copy-Item -LiteralPath (Join-Path $repositoryRoot "LICENSE") -Destination $releaseLicense
Copy-Item -LiteralPath (Join-Path $repositoryRoot "tools\renium\renium-agents.md") -Destination $releaseAgentInstructions
Copy-Item -LiteralPath (Join-Path $repositoryRoot "tools\renium\renium-guides") -Destination $releaseAgentGuides -Recurse
$releaseSupportFiles = @(
    $releaseReadme,
    $releaseLicense,
    $releaseAgentInstructions
)
$releaseSupportFiles += Get-ChildItem -LiteralPath $releaseAgentGuides -File | ForEach-Object FullName
if ($env:OS -eq "Windows_NT") {
    $releaseRbx = Join-Path $releaseDirectory "rbx.cmd"
    $releaseInstaller = Join-Path $releaseDirectory "install.ps1"
    $releaseInstallerLauncher = Join-Path $releaseDirectory "Install Renium.cmd"
    Copy-Item -LiteralPath (Join-Path $repositoryRoot "rbx.cmd") -Destination $releaseRbx
    Copy-Item -LiteralPath (Join-Path $repositoryRoot "install.ps1") -Destination $releaseInstaller
    & (Join-Path $repositoryRoot "tools\build-windows-launcher.ps1") `
        -InstallerScript $releaseInstaller `
        -Version $cliVersion `
        -OutputPath $releaseInstallerLauncher
    $releaseSupportFiles += @($releaseRbx, $releaseInstaller, $releaseInstallerLauncher)
}
else {
    $releaseRbx = Join-Path $releaseDirectory "rbx"
    $releaseInstaller = Join-Path $releaseDirectory "install.sh"
    Copy-Item -LiteralPath (Join-Path $repositoryRoot "rbx") -Destination $releaseRbx
    Copy-Item -LiteralPath (Join-Path $repositoryRoot "install.sh") -Destination $releaseInstaller
    $releaseSupportFiles += @($releaseRbx, $releaseInstaller)
    if ($IsMacOS) {
        $releaseInstallerLauncher = Join-Path $releaseDirectory "Install Renium.command"
        Copy-Item -LiteralPath (Join-Path $repositoryRoot "tools\renium\install-macos.command") -Destination $releaseInstallerLauncher
        $releaseSupportFiles += $releaseInstallerLauncher
    }
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

$env:RENIUM_PLUGIN_BUNDLE = $releasePluginBinary
$env:RENIUM_CLI_BUILD = $cliBinary
$env:RENIUM_INSERTABLE_OBJECTS_ICON_PATH = Join-Path $releaseDirectory "no-studio-icons"
$env:RENIUM_SKIP_PLUGIN_SCHEMA_WRITE = "1"
$env:RENIUM_CLI_TARGET_PLATFORM = $releaseTarget.Platform
$env:RENIUM_CLI_TARGET_ARCH = $releaseTarget.Architecture

Copy-ExtensionStage -Source $extensionDirectory -Destination $extensionStage
Invoke-Checked -File $npm -Arguments @("ci", "--prefix", $extensionStage) -WorkingDirectory $repositoryRoot
if (-not $SkipTests) {
    Invoke-Checked -File $npm -Arguments @("--prefix", $extensionStage, "run", "verify") -WorkingDirectory $repositoryRoot
}
$extensionPlatform = $releaseTarget.Platform
$extensionArchitecture = $releaseTarget.Architecture
$extensionTarget = "$extensionPlatform-$extensionArchitecture"
$releaseVsix = Join-Path $releaseDirectory ("renium-" + $cliVersion + "-" + $extensionTarget + ".vsix")
Invoke-Checked -File $npx -Arguments @("--no-install", "vsce", "package", "--target", $extensionTarget, "--out", $releaseVsix) -WorkingDirectory $extensionStage
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
    $extensionCliEntryPath = "extension/bin/$extensionPlatform-$extensionArchitecture/$binaryName"
    $extensionCliEntry = $archive.GetEntry($extensionCliEntryPath)
    if ($null -eq $extensionCliEntry) {
        throw "Packaged VSIX is missing $extensionCliEntryPath"
    }
    $extensionAgentEntryPath = "extension/bin/$extensionPlatform-$extensionArchitecture/renium-agents.md"
    if ($null -eq $archive.GetEntry($extensionAgentEntryPath)) {
        throw "Packaged VSIX is missing $extensionAgentEntryPath"
    }
    $extensionGuideEntryPath = "extension/bin/$extensionPlatform-$extensionArchitecture/renium-guides/advanced.md"
    if ($null -eq $archive.GetEntry($extensionGuideEntryPath)) {
        throw "Packaged VSIX is missing $extensionGuideEntryPath"
    }
    if ($null -eq $archive.GetEntry("extension/resources/RENIUM/opencloud.md")) {
        throw "Packaged VSIX is missing its generated Renium topic guides"
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
    $extensionCliStream = $extensionCliEntry.Open()
    $cliSha256 = [Security.Cryptography.SHA256]::Create()
    try {
        $packagedCliHash = [BitConverter]::ToString($cliSha256.ComputeHash($extensionCliStream)).Replace("-", "").ToLowerInvariant()
    }
    finally {
        $cliSha256.Dispose()
        $extensionCliStream.Dispose()
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
$releaseCliHash = (Get-FileHash -LiteralPath $releaseCliBinary -Algorithm SHA256).Hash.ToLowerInvariant()
if ($packagedCliHash -ne $releaseCliHash) {
    throw "Packaged VSIX CLI does not match the release CLI"
}

$pluginInputs = @(
    Get-ChildItem -LiteralPath $pluginDirectory -File -Recurse |
        Where-Object {
            $_.Name -notin @("Renium.rbxm", "Renium.rbxmx")
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
    generatedAtUtc = $generatedAtUtc
    toolchain = [ordered]@{
        cargo = Invoke-CapturedChecked -File "cargo" -Arguments @("--version")
        node = Invoke-CapturedChecked -File "node" -Arguments @("--version")
        rojo = Invoke-CapturedChecked -File $rojo -Arguments @("--version") -WorkingDirectory $repositoryRoot
        vsce = Invoke-CapturedChecked -File $npx -Arguments @("--no-install", "vsce", "--version") -WorkingDirectory $extensionStage
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
}
catch {
    $buildError = $_
}
finally {
    try {
        if (Test-Path -LiteralPath $extensionStage) {
            Remove-Item -LiteralPath $extensionStage -Recurse -Force
        }
    }
    catch {
        $restoreErrors += "$($extensionStage): $($_.Exception.Message)"
    }
    $env:RENIUM_PLUGIN_BUNDLE = $previousPluginBundle
    $env:RENIUM_CLI_BUILD = $previousCliBuild
    $env:RENIUM_INSERTABLE_OBJECTS_ICON_PATH = $previousIconPath
    $env:RENIUM_SKIP_PLUGIN_SCHEMA_WRITE = $previousSkipPluginSchema
    $env:RENIUM_CLI_TARGET_PLATFORM = $previousTargetPlatform
    $env:RENIUM_CLI_TARGET_ARCH = $previousTargetArchitecture
}

if ($null -ne $buildError -or $restoreErrors.Count -gt 0) {
    if (Test-Path -LiteralPath $releaseDirectory) {
        Remove-Item -LiteralPath $releaseDirectory -Recurse -Force -ErrorAction SilentlyContinue
    }
    if ($null -ne $buildError) {
        if ($restoreErrors.Count -gt 0) {
            throw "$($buildError.Exception.Message)`nTemporary staging cleanup also failed: $($restoreErrors -join '; ')"
        }
        throw $buildError
    }
    throw "Temporary staging cleanup failed: $($restoreErrors -join '; ')"
}

Move-Item -LiteralPath $releaseDirectory -Destination $finalReleaseDirectory

Write-Host "Release artifacts verified: $finalReleaseDirectory"
if ($LocalBuild) {
    Write-Warning "This is a local/private build. It bypassed clean-checkout, license, and publisher guards and must not be published as a public release."
}
