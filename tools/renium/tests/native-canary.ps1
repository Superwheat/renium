param(
    [string]$Renium = (Join-Path $PSScriptRoot '../target/debug/renium.exe'),
    [string]$ScratchRoot = (Join-Path $PSScriptRoot '../../../.native-canary'),
    [string]$Studio,
    [string]$Place = (Join-Path $PSScriptRoot 'fixtures/native-canary.rbxlx'),
    [string]$Channel = 'LIVE'
)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
Set-StrictMode -Version Latest
Add-Type -AssemblyName System.IO.Compression.FileSystem

# Deployment packages are data, not installers. Never invoke a bootstrapper,
# register this installation, change Studio settings, or replace Renium.
$destinations = @{
    'RobloxStudio.zip' = ''; 'Libraries.zip' = ''; 'LibrariesQt5.zip' = ''
    'WebView2.zip' = ''; 'WebView2RuntimeInstaller.zip' = 'WebView2RuntimeInstaller'
    'redist.zip' = ''; 'shaders.zip' = 'shaders'; 'ssl.zip' = 'ssl'
    'ApplicationConfig.zip' = 'ApplicationConfig'; 'Plugins.zip' = 'Plugins'
    'BuiltInPlugins.zip' = 'BuiltInPlugins'; 'BuiltInStandalonePlugins.zip' = 'BuiltInStandalonePlugins'
    'StudioFonts.zip' = 'StudioFonts'; 'Qml.zip' = 'Qml'; 'RibbonConfig.zip' = 'RibbonConfig'
    'content-avatar.zip' = 'content/avatar'; 'content-configs.zip' = 'content/configs'
    'content-fonts.zip' = 'content/fonts'; 'content-models.zip' = 'content/models'
    'content-qt_translations.zip' = 'content/qt_translations'; 'content-sky.zip' = 'content/sky'
    'content-sounds.zip' = 'content/sounds'; 'content-textures2.zip' = 'content/textures'
    'content-textures3.zip' = 'PlatformContent/pc/textures'
    'content-studio_svg_textures.zip' = 'content/studio_svg_textures'
    'content-terrain.zip' = 'PlatformContent/pc/terrain'
    'content-platform-fonts.zip' = 'PlatformContent/pc/fonts'
    'content-platform-dictionaries.zip' = 'PlatformContent/pc/shared_compression_dictionaries'
    'content-api-docs.zip' = 'content/api_docs'
    'extracontent-scripts.zip' = 'ExtraContent/scripts'
    'extracontent-luapackages.zip' = 'ExtraContent/LuaPackages'
    'extracontent-translations.zip' = 'ExtraContent/translations'
    'extracontent-models.zip' = 'ExtraContent/models'; 'extracontent-textures.zip' = 'ExtraContent/textures'
    'extracontent-places.zip' = 'ExtraContent/places'
    'studiocontent-models.zip' = 'StudioContent/models'; 'studiocontent-textures.zip' = 'StudioContent/textures'
}

function Expand-DeploymentPackage([string]$Archive, [string]$Destination) {
    $root = [IO.Path]::GetFullPath($Destination).TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar
    [IO.Directory]::CreateDirectory($root) | Out-Null
    $zip = [IO.Compression.ZipFile]::OpenRead($Archive)
    try {
        foreach ($entry in $zip.Entries) {
            $target = [IO.Path]::GetFullPath((Join-Path $root $entry.FullName))
            if (-not $target.StartsWith($root, [StringComparison]::OrdinalIgnoreCase)) {
                throw "Deployment archive path escapes its directory: $($entry.FullName)"
            }
            if ($entry.FullName.EndsWith('/') -or $entry.FullName.EndsWith('\')) {
                [IO.Directory]::CreateDirectory($target) | Out-Null
            } else {
                [IO.Directory]::CreateDirectory([IO.Path]::GetDirectoryName($target)) | Out-Null
                [IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $target, $true)
            }
        }
    } finally { $zip.Dispose() }
}

$Renium = (Resolve-Path -LiteralPath $Renium).Path
$Place = (Resolve-Path -LiteralPath $Place).Path
$runName = 'renium-native-canary-' + [guid]::NewGuid().ToString('N')
$runRoot = [IO.Path]::GetFullPath((Join-Path $ScratchRoot $runName))
[IO.Directory]::CreateDirectory($runRoot) | Out-Null
$reportPath = Join-Path $runRoot 'probes.json'
$report = $null
$previousDaemon = $env:RENIUM_DAEMON_NAME
$env:RENIUM_DAEMON_NAME = 'astra'
try {
    if (-not $Studio) {
        if ($Channel -notmatch '^[A-Za-z0-9_-]+$') { throw 'Invalid deployment channel' }
        $versionUrl = "https://clientsettingscdn.roblox.com/v2/client-version/WindowsStudio64/channel/$Channel"
        $version = Invoke-RestMethod -Uri $versionUrl -TimeoutSec 120
        $version | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $runRoot 'deployment.json') -Encoding utf8
        $upload = $version.clientVersionUpload
        if ($upload -notmatch '^version-[a-fA-F0-9]+$') { throw 'Unexpected deployment version identifier' }
        $cdn = 'https://setup.rbxcdn.com'
        if ($Channel -ne 'LIVE') { $cdn += "/channel/$Channel" }
        $manifest = (Invoke-WebRequest -UseBasicParsing -Uri "$cdn/$upload-rbxPkgManifest.txt" -TimeoutSec 120).Content
        $manifest | Set-Content -LiteralPath (Join-Path $runRoot 'rbxPkgManifest.txt') -Encoding utf8
        $lines = @($manifest -split '\r?\n' | Where-Object { $_ -ne '' })
        if ($lines[0] -ne 'v0' -or ($lines.Count - 1) % 4 -ne 0) { throw 'Unknown deployment manifest format' }
        $install = Join-Path $runRoot 'studio'
        for ($i = 1; $i -lt $lines.Count; $i += 4) {
            $name = $lines[$i]
            if (-not $destinations.ContainsKey($name)) { throw "Unknown package destination: $name; update the canary map" }
            $archive = Join-Path $runRoot $name
            Write-Output "Deploying $name"
            Invoke-WebRequest -UseBasicParsing -Uri "$cdn/$upload-$name" -OutFile $archive -TimeoutSec 300
            if ((Get-Item -LiteralPath $archive).Length -ne [long]$lines[$i + 2] -or
                (Get-FileHash -LiteralPath $archive -Algorithm MD5).Hash -ne $lines[$i + 1]) {
                throw "Deployment checksum or length mismatch: $name"
            }
            Expand-DeploymentPackage $archive (Join-Path $install $destinations[$name])
            Remove-Item -LiteralPath $archive
        }
        $Studio = Join-Path $install 'RobloxStudioBeta.exe'
        '<Settings><ContentFolder>content</ContentFolder><BaseUrl>https://www.roblox.com</BaseUrl></Settings>' |
            Set-Content -LiteralPath (Join-Path $install 'AppSettings.xml') -Encoding utf8
    }
    $Studio = (Resolve-Path -LiteralPath $Studio).Path
    $scratchPlace = Join-Path $runRoot ($runName + [IO.Path]::GetExtension($Place))
    Copy-Item -LiteralPath $Place -Destination $scratchPlace
    # Renium's launcher preserves foreground ownership; no Start-Process focus race.
    $output = & $Renium --daemon astra native-probe --launch $Studio --file $scratchPlace
    $code = $LASTEXITCODE
    $output | Set-Content -LiteralPath $reportPath -Encoding utf8
    $report = $output | ConvertFrom-Json
    $rows = @('| Operation | Verdict | Reason |', '| --- | --- | --- |')
    foreach ($row in $report.operations) {
        $reason = ($row.reason -replace '[\r\n|]', ' ')
        $rows += "| $($row.operation) | $($row.verdict) | $reason |"
    }
    $version = (Get-Item -LiteralPath $Studio).VersionInfo.FileVersion
    $summary = @("Studio $version; PID $($report.pid)", '', $rows, '', "Evidence: $reportPath")
    $summary | Set-Content -LiteralPath (Join-Path $runRoot 'summary.md') -Encoding utf8
    $summary | Write-Output
    if ($env:GITHUB_STEP_SUMMARY) { $summary | Add-Content -LiteralPath $env:GITHUB_STEP_SUMMARY }
    if ($code -ne 0 -or -not $report.ok) { throw "Native canary failed; see the per-operation table and $reportPath" }
} finally {
    # Only the exact process launched with this run's unique place may be stopped.
    $owned = @(Get-Process RobloxStudioBeta -ErrorAction SilentlyContinue | Where-Object {
        $_.MainWindowTitle.Contains($runName) -and $_.Path -eq $Studio
    })
    foreach ($process in $owned) { Stop-Process -Id $process.Id -Force }
    $env:RENIUM_DAEMON_NAME = $previousDaemon
    Write-Output "Canary artifacts: $runRoot"
}
