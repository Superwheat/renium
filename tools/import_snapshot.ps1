param(
    [string]$SnapshotDir = ".\snapshots",
    [string]$ProjectRoot = (Resolve-Path ".").Path,
    [int]$ProgressEvery = 250,
    [switch]$CompactMetaJson,
    [switch]$SkipDefaultFiltering,
    [string[]]$Services = @(),
    [switch]$NoProjectWrite
)

$ErrorActionPreference = "Stop"

$DefaultTargetServices = @(
    "Workspace",
    "Players",
    "Lighting",
    "MaterialService",
    "ReplicatedFirst",
    "ReplicatedStorage",
    "ServerScriptService",
    "ServerStorage",
    "StarterGui",
    "StarterPack",
    "StarterPlayer"
)

$TargetServices = @($DefaultTargetServices)
if ($Services -and $Services.Count -gt 0) {
    $serviceLookup = @{}
    foreach ($svc in $DefaultTargetServices) {
        $serviceLookup[$svc] = $true
    }

    $selected = @()
    foreach ($rawSvc in $Services) {
        $svc = [string]$rawSvc
        if (-not $svc) {
            continue
        }
        if (-not $serviceLookup.ContainsKey($svc)) {
            throw "Unsupported service in -Services: $svc"
        }
        if ($selected -notcontains $svc) {
            $selected += $svc
        }
    }
    if ($selected.Count -eq 0) {
        throw "No valid services provided in -Services."
    }
    $TargetServices = $selected
}

if (-not (Test-Path -LiteralPath $SnapshotDir)) {
    throw "Snapshot directory not found: $SnapshotDir"
}

$srcRoot = Join-Path $ProjectRoot "src"
New-Item -ItemType Directory -Force -Path $srcRoot | Out-Null
$projectRootFullPath = [System.IO.Path]::GetFullPath($ProjectRoot)
$trimmedProjectRoot = $projectRootFullPath.TrimEnd([char[]]@(
    [System.IO.Path]::DirectorySeparatorChar,
    [System.IO.Path]::AltDirectorySeparatorChar
))
$projectName = [System.IO.Path]::GetFileName($trimmedProjectRoot)
if ([string]::IsNullOrWhiteSpace($projectName)) {
    $projectName = "ReniumProject"
}

function Sanitize-Name {
    param([string]$Name)
    $invalid = [System.IO.Path]::GetInvalidFileNameChars() -join ""
    $pattern = "[" + [Regex]::Escape($invalid) + "]"
    $sanitized = [Regex]::Replace([string]$Name, $pattern, "_")
    $sanitized = $sanitized.TrimEnd(" ", ".")
    if ($sanitized -match '^(?i:CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])$') {
        $sanitized = "_" + $sanitized
    }
    if ([string]::IsNullOrWhiteSpace($sanitized)) {
        return "_"
    }
    return $sanitized
}

function Ensure-Directory {
    param([string]$Path)
    [System.IO.Directory]::CreateDirectory($Path) | Out-Null
}

$Utf8NoBom = New-Object System.Text.UTF8Encoding($false)

function Write-Utf8File {
    param(
        [Parameter(Mandatory = $true)][string]$Path,
        [Parameter(Mandatory = $true)][AllowEmptyString()][string]$Content
    )
    [System.IO.File]::WriteAllText($Path, $Content, $Utf8NoBom)
}

function Format-JsonTextPretty {
    param(
        [Parameter(Mandatory = $true)][string]$Json,
        [int]$IndentSize = 2
    )

    $sb = New-Object System.Text.StringBuilder
    $indent = 0
    $inString = $false
    $escaping = $false

    foreach ($ch in $Json.ToCharArray()) {
        if ($inString) {
            [void]$sb.Append($ch)
            if ($escaping) {
                $escaping = $false
            } elseif ($ch -eq '\') {
                $escaping = $true
            } elseif ($ch -eq '"') {
                $inString = $false
            }
            continue
        }

        switch ($ch) {
            '"' {
                $inString = $true
                [void]$sb.Append($ch)
            }
            '{' {
                [void]$sb.Append($ch)
                $indent += 1
                [void]$sb.Append("`n")
                [void]$sb.Append((" " * ($indent * $IndentSize)))
            }
            '[' {
                [void]$sb.Append($ch)
                $indent += 1
                [void]$sb.Append("`n")
                [void]$sb.Append((" " * ($indent * $IndentSize)))
            }
            '}' {
                $indent = [Math]::Max(0, $indent - 1)
                [void]$sb.Append("`n")
                [void]$sb.Append((" " * ($indent * $IndentSize)))
                [void]$sb.Append($ch)
            }
            ']' {
                $indent = [Math]::Max(0, $indent - 1)
                [void]$sb.Append("`n")
                [void]$sb.Append((" " * ($indent * $IndentSize)))
                [void]$sb.Append($ch)
            }
            ',' {
                [void]$sb.Append($ch)
                [void]$sb.Append("`n")
                [void]$sb.Append((" " * ($indent * $IndentSize)))
            }
            ':' {
                [void]$sb.Append(": ")
            }
            default {
                [void]$sb.Append($ch)
            }
        }
    }

    return $sb.ToString()
}

function Write-MetaJsonFile {
    param(
        [Parameter(Mandatory = $true)]$Value,
        [Parameter(Mandatory = $true)][string]$Path,
        [int]$Depth = 40
    )
    $raw = $Value | ConvertTo-Json -Depth $Depth -Compress
    if ($CompactMetaJson) {
        Write-Utf8File -Path $Path -Content $raw
        return
    }
    $formatted = Format-JsonTextPretty -Json $raw -IndentSize 2
    $formatted = Compress-InlineRojoValues -Json $formatted
    Write-Utf8File -Path $Path -Content $formatted
}

function Compress-InlineRojoValues {
    param(
        [Parameter(Mandatory = $true)][string]$Json
    )

    $inlineValueKinds = "Vector2|Vector3|UDim|UDim2|Color3|CFrame|Rect"
    $pattern = '(?s)\{\s*"(?<kind>' + $inlineValueKinds + ')"\s*:\s*\[(?<arr>.*?)\]\s*\}'

    return [Regex]::Replace($Json, $pattern, {
        param($m)
        $kind = $m.Groups["kind"].Value
        $arr = $m.Groups["arr"].Value -replace '\s+', ''
        return '{"' + $kind + '":[' + $arr + ']}'
    })
}

function Test-DictionaryKey {
    param(
        $Obj,
        [string]$Name
    )

    if ($null -eq $Obj -or -not ($Obj -is [System.Collections.IDictionary])) {
        return $false
    }

    $containsKeyMethod = $Obj.PSObject.Methods["ContainsKey"]
    if ($containsKeyMethod) {
        return [bool]$Obj.ContainsKey($Name)
    }

    $containsMethod = $Obj.PSObject.Methods["Contains"]
    if ($containsMethod) {
        return [bool]$Obj.Contains($Name)
    }

    return $false
}

function Get-Field {
    param($Obj, [string]$Name)

    if ($null -eq $Obj) { return $null }

    if ($Obj -is [System.Collections.IDictionary]) {
        if (Test-DictionaryKey -Obj $Obj -Name $Name) { return $Obj[$Name] }
        return $null
    }

    $prop = $Obj.PSObject.Properties[$Name]
    if ($prop) { return $prop.Value }
    return $null
}

function Has-Field {
    param($Obj, [string]$Name)

    if ($null -eq $Obj) { return $false }

    if ($Obj -is [System.Collections.IDictionary]) {
        return (Test-DictionaryKey -Obj $Obj -Name $Name)
    }

    return ($null -ne $Obj.PSObject.Properties[$Name])
}

function ConvertFrom-JsonSafe {
    param(
        [Parameter(Mandatory = $true)][string]$JsonText
    )

    try {
        return $JsonText | ConvertFrom-Json
    } catch {
        $message = $_.Exception.Message
        if ($message -notmatch "duplicated keys") {
            throw
        }

        Add-Type -AssemblyName System.Web.Extensions
        $serializer = New-Object System.Web.Script.Serialization.JavaScriptSerializer
        $serializer.MaxJsonLength = [int]::MaxValue
        return $serializer.DeserializeObject($JsonText)
    }
}

function Convert-ValueToRojo {
    param($Value)

    if ($null -eq $Value) {
        return $null
    }

    if ($Value -is [string] -or $Value -is [bool] -or $Value -is [int] -or $Value -is [long] -or $Value -is [double] -or $Value -is [float] -or $Value -is [decimal]) {
        return $Value
    }

    if (Has-Field -Obj $Value -Name "_type") {
        $t = [string](Get-Field -Obj $Value -Name "_type")

        if ($t -eq "Vector2") {
            return @([double](Get-Field $Value "x"), [double](Get-Field $Value "y"))
        }
        if ($t -eq "Vector3") {
            return @([double](Get-Field $Value "x"), [double](Get-Field $Value "y"), [double](Get-Field $Value "z"))
        }
        if ($t -eq "UDim") {
            return @([double](Get-Field $Value "scale"), [int](Get-Field $Value "offset"))
        }
        if ($t -eq "UDim2") {
            return @(
                @([double](Get-Field $Value "xScale"), [int](Get-Field $Value "xOffset")),
                @([double](Get-Field $Value "yScale"), [int](Get-Field $Value "yOffset"))
            )
        }
        if ($t -eq "Color3") {
            return @([double](Get-Field $Value "r"), [double](Get-Field $Value "g"), [double](Get-Field $Value "b"))
        }
        if ($t -eq "CFrame") {
            $components = @()
            foreach ($n in (Get-Field $Value "components")) {
                $components += [double]$n
            }
            return $components
        }
        if ($t -eq "Rect") {
            return @(
                [double](Get-Field $Value "minX"),
                [double](Get-Field $Value "minY"),
                [double](Get-Field $Value "maxX"),
                [double](Get-Field $Value "maxY")
            )
        }
        if ($t -eq "EnumItem") {
            return [string](Get-Field $Value "name")
        }
        if ($t -eq "Font") {
            $weightRaw = [string](Get-Field $Value "weight")
            $styleRaw = [string](Get-Field $Value "style")
            $weight = ($weightRaw -split '\.')[-1]
            $style = ($styleRaw -split '\.')[-1]
            return @{
                family = [string](Get-Field $Value "family")
                weight = $weight
                style = $style
                cachedFaceId = $null
            }
        }
        if ($t -eq "NumberSequence") {
            $out = @()
            foreach ($kp in (Get-Field $Value "keypoints")) {
                $out += @{
                    time = [double](Get-Field $kp "time")
                    value = [double](Get-Field $kp "value")
                    envelope = [double](Get-Field $kp "envelope")
                }
            }
            return $out
        }
        if ($t -eq "ColorSequence") {
            $out = @()
            foreach ($kp in (Get-Field $Value "keypoints")) {
                $kv = Get-Field $kp "value"
                $out += @{
                    time = [double](Get-Field $kp "time")
                    color = @(
                        [double](Get-Field $kv "r"),
                        [double](Get-Field $kv "g"),
                        [double](Get-Field $kv "b")
                    )
                }
            }
            return $out
        }

        return $null
    }

    return $null
}

function Convert-AttributesToRojo {
    param($Attributes)

    if ($null -eq $Attributes) {
        return $null
    }

    $pairs = @{}

    if ($Attributes -is [System.Collections.IDictionary]) {
        foreach ($k in $Attributes.Keys) {
            $v = $Attributes[$k]
            if ($v -is [bool]) {
                $pairs[$k] = @{ Bool = $v }
            } elseif ($v -is [string]) {
                $pairs[$k] = @{ String = $v }
            } elseif ($v -is [int] -or $v -is [long] -or $v -is [double] -or $v -is [float] -or $v -is [decimal]) {
                $pairs[$k] = @{ Float64 = [double]$v }
            }
        }
    }

    if ($pairs.Count -eq 0) {
        return $null
    }

    return @{ Attributes = $pairs }
}

function Is-DefaultMetaProperty {
    param(
        [string]$ClassName,
        [string]$PropertyName,
        $PropertyValue
    )

    if ($PropertyName -eq "Archivable" -and $PropertyValue -eq $true) {
        return $true
    }

    if ($PropertyName -eq "Enabled" -and $PropertyValue -eq $true) {
        return $true
    }

    if ($PropertyName -eq "Disabled" -and $PropertyValue -eq $false) {
        return $true
    }

    if ($PropertyName -eq "LinkedSource" -and $PropertyValue -is [string] -and [string]::IsNullOrEmpty($PropertyValue)) {
        return $true
    }

    if ($PropertyName -eq "RunContext" -and $ClassName -eq "Script" -and [string]$PropertyValue -eq "Legacy") {
        return $true
    }

    return $false
}

function Convert-ToCanonicalJson {
    param($Value)
    if ($null -eq $Value) {
        return "null"
    }
    return ($Value | ConvertTo-Json -Depth 60 -Compress)
}

function Merge-ClassDefaults {
    param(
        $Snapshot,
        [hashtable]$ClassDefaultsByClass
    )

    if (-not (Has-Field -Obj $Snapshot -Name "classDefaults")) {
        return
    }

    $classDefaults = Get-Field -Obj $Snapshot -Name "classDefaults"
    if ($null -eq $classDefaults) {
        return
    }

    if ($classDefaults -is [System.Collections.IDictionary]) {
        foreach ($className in $classDefaults.Keys) {
            if ($className) {
                $ClassDefaultsByClass[[string]$className] = $classDefaults[$className]
            }
        }
        return
    }

    foreach ($prop in $classDefaults.PSObject.Properties) {
        $className = [string]$prop.Name
        if ($className) {
            $ClassDefaultsByClass[$className] = $prop.Value
        }
    }
}

function Is-DefaultRawProperty {
    param(
        [string]$ClassName,
        [string]$PropertyName,
        $RawValue,
        [hashtable]$ClassDefaultsByClass
    )

    if (-not $ClassDefaultsByClass.ContainsKey($ClassName)) {
        return $false
    }

    $classDefaults = $ClassDefaultsByClass[$ClassName]
    if (-not (Has-Field -Obj $classDefaults -Name $PropertyName)) {
        return $false
    }

    $defaultRawValue = Get-Field -Obj $classDefaults -Name $PropertyName
    return (Convert-ToCanonicalJson -Value $RawValue) -eq (Convert-ToCanonicalJson -Value $defaultRawValue)
}

$allInstances = New-Object System.Collections.Generic.List[object]
$instancesByPath = @{}
$instancesByDebugId = @{}
$serviceRootByName = @{}
$childrenByParentPath = @{}
$childrenByParentDebugId = @{}
$classDefaultsByClass = @{}
$instanceOrdinal = 0
$script:VisitedInstanceKeys = @{}

function Add-ChildLink {
    param(
        [hashtable]$Table,
        [string]$ParentKey,
        $Child
    )
    if (-not $ParentKey) {
        return
    }
    if (-not $Table.ContainsKey($ParentKey)) {
        $Table[$ParentKey] = New-Object System.Collections.Generic.List[object]
    }
    $Table[$ParentKey].Add($Child)
}

function Get-InstanceKey {
    param($Instance)
    $existing = Get-Field -Obj $Instance -Name "__cdxInstanceKey"
    if ($existing) {
        return [string]$existing
    }

    $debugId = Get-Field -Obj $Instance -Name "debugId"
    if ($debugId) {
        return "debug:$debugId"
    }

    return "path:$([string](Get-Field -Obj $Instance -Name 'path'))"
}

function Get-ChildrenForInstance {
    param($Instance)

    $path = [string](Get-Field -Obj $Instance -Name "path")
    $debugId = ""
    if (Has-Field -Obj $Instance -Name "debugId") {
        $debugIdValue = Get-Field -Obj $Instance -Name "debugId"
        if ($debugIdValue) {
            $debugId = [string]$debugIdValue
        }
    }

    $rawChildren = $null
    if ($debugId -and $childrenByParentDebugId.ContainsKey($debugId)) {
        $rawChildren = $childrenByParentDebugId[$debugId]
    } elseif ($path -and $childrenByParentPath.ContainsKey($path)) {
        $rawChildren = $childrenByParentPath[$path]
    }

    if (-not $rawChildren) {
        return @()
    }

    $deduped = New-Object System.Collections.Generic.List[object]
    $seen = @{}
    foreach ($child in $rawChildren) {
        $childKey = Get-InstanceKey -Instance $child
        if (-not $seen.ContainsKey($childKey)) {
            $seen[$childKey] = $true
            $deduped.Add($child)
        }
    }
    return $deduped
}

function Read-SnapshotInstances {
    param(
        $Snapshot,
        [string]$SnapshotDir,
        [string]$Service
    )

    $seen = @{}

    function Emit-IfNew {
        param($Instance)

        if ($null -eq $Instance) {
            return
        }

        $dedupeKey = ""
        $path = [string](Get-Field -Obj $Instance -Name "path")
        if ($path) {
            $dedupeKey = "path:$path"
        } else {
            $debugId = [string](Get-Field -Obj $Instance -Name "debugId")
            if ($debugId) {
                $dedupeKey = "debug:$debugId"
            }
        }

        if ($dedupeKey) {
            if ($seen.ContainsKey($dedupeKey)) {
                return
            }
            $seen[$dedupeKey] = $true
        }

        $Instance
    }

    $baseInstances = Get-Field -Obj $Snapshot -Name "instances"
    if ($null -ne $baseInstances) {
        foreach ($instance in $baseInstances) {
            Emit-IfNew -Instance $instance
        }
    }

    $chunkEntries = $null
    if (Has-Field -Obj $Snapshot -Name "instanceChunks") {
        $chunkEntries = Get-Field -Obj $Snapshot -Name "instanceChunks"
    }
    if ($null -eq $chunkEntries) {
        return
    }

    $chunks = @($chunkEntries)
    $totalChunks = $chunks.Count
    $chunkIndex = 0
    foreach ($entry in $chunks) {
        $chunkIndex += 1
        $fileName = ""
        if ($entry -is [string]) {
            $fileName = [string]$entry
        } elseif (Has-Field -Obj $entry -Name "file") {
            $fileName = [string](Get-Field -Obj $entry -Name "file")
        }

        if (-not $fileName) {
            throw "Snapshot chunk entry missing file for $Service (chunk $chunkIndex/$totalChunks)"
        }

        $chunkPath = Join-Path $SnapshotDir $fileName
        if (-not (Test-Path -LiteralPath $chunkPath)) {
            throw "Missing snapshot chunk file: $chunkPath"
        }

        $chunkRaw = Get-Content -LiteralPath $chunkPath -Raw
        $chunkParsed = ConvertFrom-JsonSafe -JsonText $chunkRaw

        $chunkInstances = $null
        if (Has-Field -Obj $chunkParsed -Name "instances") {
            $chunkInstances = Get-Field -Obj $chunkParsed -Name "instances"
        } else {
            $chunkInstances = $chunkParsed
        }

        if ($null -eq $chunkInstances) {
            continue
        }

        foreach ($instance in $chunkInstances) {
            Emit-IfNew -Instance $instance
        }

        if ($totalChunks -le 10 -or $chunkIndex % 10 -eq 0 -or $chunkIndex -eq $totalChunks) {
            Write-Host ("[sync] loaded {0} chunk {1}/{2}" -f $Service, $chunkIndex, $totalChunks)
        }
    }
}

foreach ($service in $TargetServices) {
    $filePath = Join-Path $SnapshotDir ("$service.json")
    if (-not (Test-Path -LiteralPath $filePath)) {
        throw "Missing snapshot file: $filePath"
    }

    $snapshotRaw = Get-Content -LiteralPath $filePath -Raw
    $snapshot = ConvertFrom-JsonSafe -JsonText $snapshotRaw
    if (-not $SkipDefaultFiltering) {
        Merge-ClassDefaults -Snapshot $snapshot -ClassDefaultsByClass $classDefaultsByClass
    }

    $rootPath = ""
    if (Has-Field -Obj $snapshot -Name "services") {
        $servicesNode = Get-Field -Obj $snapshot -Name "services"
        if ($servicesNode -and $servicesNode.Count -gt 0) {
            $svc0 = $servicesNode[0]
            if (Has-Field -Obj $svc0 -Name "path") {
                $p = Get-Field -Obj $svc0 -Name "path"
                if ($p) {
                    $rootPath = [string]$p
                }
            }
        }
    }

    if (-not $rootPath) {
        if ($instancesByPath.ContainsKey("game.$service")) {
            $rootPath = "game.$service"
        } else {
            $rootPath = $service
        }
    }

    $serviceRoot = $null
    foreach ($instance in (Read-SnapshotInstances -Snapshot $snapshot -SnapshotDir $SnapshotDir -Service $service)) {
        $instanceOrdinal += 1
        $instanceKey = "idx:$instanceOrdinal"
        $instance | Add-Member -NotePropertyName "__cdxInstanceKey" -NotePropertyValue $instanceKey -Force
        $allInstances.Add($instance)

        $path = [string](Get-Field -Obj $instance -Name "path")
        if ($path -and -not $instancesByPath.ContainsKey($path)) {
            $instancesByPath[$path] = $instance
        }

        $debugId = ""
        if (Has-Field -Obj $instance -Name "debugId") {
            $debugIdValue = Get-Field -Obj $instance -Name "debugId"
            if ($debugIdValue) {
                $debugId = [string]$debugIdValue
                $instance | Add-Member -NotePropertyName "__cdxInstanceKey" -NotePropertyValue ("debug:" + $debugId + "|idx:" + $instanceOrdinal) -Force
                if (-not $instancesByDebugId.ContainsKey($debugId)) {
                    $instancesByDebugId[$debugId] = $instance
                }
            }
        }

        if ($path -and $path -eq $rootPath -and -not $serviceRoot) {
            $serviceRoot = $instance
        }

        $parentDebugId = ""
        if (Has-Field -Obj $instance -Name "parentDebugId") {
            $pd = Get-Field -Obj $instance -Name "parentDebugId"
            if ($pd) {
                $parentDebugId = [string]$pd
            }
        }
        if ($parentDebugId) {
            Add-ChildLink -Table $childrenByParentDebugId -ParentKey $parentDebugId -Child $instance
        }

        $parentPath = ""
        if (Has-Field -Obj $instance -Name "parentPath") {
            $pp = Get-Field -Obj $instance -Name "parentPath"
            if ($pp) {
                $parentPath = [string]$pp
            }
        }

        if (-not $parentPath) {
            $lastDot = $path.LastIndexOf('.')
            if ($lastDot -gt 0) {
                $parentPath = $path.Substring(0, $lastDot)
            }
        }

        if ($parentPath) {
            Add-ChildLink -Table $childrenByParentPath -ParentKey $parentPath -Child $instance
        }
    }

    if (-not $serviceRoot) {
        if ($instancesByPath.ContainsKey("game.$service")) {
            $serviceRoot = $instancesByPath["game.$service"]
        } elseif ($instancesByPath.ContainsKey($service)) {
            $serviceRoot = $instancesByPath[$service]
        }
    }

    if ($serviceRoot) {
        $serviceRootByName[$service] = $serviceRoot
    }
}

function Get-ReachableInstanceCount {
    $seen = @{}

    function Visit-InstanceForCount {
        param($Node)
        if (-not $Node) {
            return
        }
        $key = Get-InstanceKey -Instance $Node
        if ($seen.ContainsKey($key)) {
            return
        }
        $seen[$key] = $true
        $children = Get-ChildrenForInstance -Instance $Node
        foreach ($child in $children) {
            Visit-InstanceForCount -Node $child
        }
    }

    foreach ($svc in $TargetServices) {
        if ($serviceRootByName.ContainsKey($svc)) {
            Visit-InstanceForCount -Node $serviceRootByName[$svc]
        }
    }

    return $seen.Count
}

$script:SyncProgressCurrent = 0
$script:SyncProgressTotal = Get-ReachableInstanceCount
$script:SyncProgressWarnedOverflow = $false
if ($script:SyncProgressTotal -le 0) {
    $script:SyncProgressTotal = $allInstances.Count
}

function Write-SyncInstanceProgress {
    param(
        [string]$InstancePath,
        [string]$ClassName
    )

    $script:SyncProgressCurrent += 1
    if ($script:SyncProgressCurrent -gt $script:SyncProgressTotal) {
        if (-not $script:SyncProgressWarnedOverflow) {
            Write-Output ("[sync] warning: progress total underestimated ({0}); adjusting live total" -f $script:SyncProgressTotal)
            $script:SyncProgressWarnedOverflow = $true
        }
        $script:SyncProgressTotal = $script:SyncProgressCurrent
    }
    if ($script:SyncProgressCurrent -eq 1 -or
        $script:SyncProgressCurrent -eq $script:SyncProgressTotal -or
        ($ProgressEvery -gt 0 -and ($script:SyncProgressCurrent % $ProgressEvery) -eq 0)) {
        Write-Output ("[sync] Syncing instance {0}/{1}: {2} ({3})" -f $script:SyncProgressCurrent, $script:SyncProgressTotal, $InstancePath, $ClassName)
    }
}

function Build-Meta {
    param(
        $Instance,
        [hashtable]$ClassDefaultsByClass
    )

    $propsOut = @{}

    if ($Instance.properties) {
        $className = [string]$Instance.className
        $propertyEntries = @()
        if ($Instance.properties -is [System.Collections.IDictionary]) {
            foreach ($key in $Instance.properties.Keys) {
                $propertyEntries += [PSCustomObject]@{
                    Name = [string]$key
                    Value = $Instance.properties[$key]
                }
            }
        } else {
            $propertyEntries = $Instance.properties.PSObject.Properties
        }

        foreach ($prop in $propertyEntries) {
            $name = [string]$prop.Name
            $lowerName = $name.ToLowerInvariant()
            if ($lowerName -eq "source" -or $lowerName -eq "classname" -or $lowerName -eq "parent" -or $lowerName -eq "name" -or $lowerName -eq "robloxlocked") {
                continue
            }
            if (-not $SkipDefaultFiltering) {
                if (Is-DefaultRawProperty -ClassName $className -PropertyName $name -RawValue $prop.Value -ClassDefaultsByClass $ClassDefaultsByClass) {
                    continue
                }
            }
            $converted = Convert-ValueToRojo -Value $prop.Value
            if ($null -ne $converted) {
                if ($name -eq "RunContext" -and $className -ne "Script") {
                    continue
                }
                if (-not $SkipDefaultFiltering) {
                    if (Is-DefaultMetaProperty -ClassName $className -PropertyName $name -PropertyValue $converted) {
                        continue
                    }
                }
                $propsOut[$name] = $converted
            }
        }
    }


    $attributes = Convert-AttributesToRojo -Attributes $Instance.attributes
    if ($null -ne $attributes) {
        $propsOut["Attributes"] = $attributes.Attributes
    }

    $meta = [ordered]@{
        className = [string]$Instance.className
    }
    if ($propsOut.Count -gt 0) {
        $meta.properties = $propsOut
    }
    return $meta
}
function Meta-HasSettings {
    param($Meta)

    if (-not $Meta -or -not (Has-Field -Obj $Meta -Name "properties")) {
        return $false
    }

    $properties = Get-Field -Obj $Meta -Name "properties"
    if ($properties -is [System.Collections.IDictionary]) {
        return ($properties.Count -gt 0)
    }

    return $false
}
function Emit-Node {
    param(
        $Instance,
        [string]$ParentDir,
        [string]$FsStem
    )

    if (-not $Instance) {
        return
    }
    $instance = $Instance
    $instanceKey = Get-InstanceKey -Instance $instance
    if ($script:VisitedInstanceKeys.ContainsKey($instanceKey)) {
        return
    }
    $script:VisitedInstanceKeys[$instanceKey] = $true

    $instancePath = [string](Get-Field -Obj $instance -Name "path")
    $children = Get-ChildrenForInstance -Instance $instance
    $hasChildren = ($children -and $children.Count -gt 0)
    $className = [string]$instance.className

    Write-SyncInstanceProgress -InstancePath $instancePath -ClassName $className

    if ($className -eq "Script" -or $className -eq "LocalScript" -or $className -eq "ModuleScript") {
        $ext = if ($className -eq "Script") { ".server.luau" } elseif ($className -eq "LocalScript") { ".client.luau" } else { ".luau" }

        $source = ""
        if ($instance.properties -and $instance.properties.Source) {
            $source = [string]$instance.properties.Source
        }

        $meta = Build-Meta -Instance $instance -ClassDefaultsByClass $classDefaultsByClass
        $hasMetaSettings = Meta-HasSettings -Meta $meta

        if ($hasChildren) {
            $dirPath = Join-Path $ParentDir $FsStem
            Ensure-Directory -Path $dirPath

            $initScript = if ($className -eq "Script") { "init.server.luau" } elseif ($className -eq "LocalScript") { "init.client.luau" } else { "init.luau" }
            Write-Utf8File -Path (Join-Path $dirPath $initScript) -Content $source
            if ($hasMetaSettings) {
                Write-MetaJsonFile -Value $meta -Path (Join-Path $dirPath "init.meta.json")
            }

            $nameCounter = @{}
            foreach ($child in $children) {
                $base = Sanitize-Name -Name ([string]$child.name)
                if (-not $nameCounter.ContainsKey($base)) {
                    $nameCounter[$base] = 0
                }
                $nameCounter[$base] += 1
                $stem = if ($nameCounter[$base] -eq 1) { $base } else { "$base`_$($nameCounter[$base])" }
                Emit-Node -Instance $child -ParentDir $dirPath -FsStem $stem
            }
        } else {
            $scriptPath = Join-Path $ParentDir ($FsStem + $ext)
            Write-Utf8File -Path $scriptPath -Content $source
            if ($hasMetaSettings) {
                $metaPath = Join-Path $ParentDir ($FsStem + ".meta.json")
                Write-MetaJsonFile -Value $meta -Path $metaPath
            }
        }

        return
    }

    $dirPath = Join-Path $ParentDir $FsStem
    Ensure-Directory -Path $dirPath

    $meta = Build-Meta -Instance $instance -ClassDefaultsByClass $classDefaultsByClass
    $initMetaPath = Join-Path $dirPath "init.meta.json"
    $hasMetaSettings = Meta-HasSettings -Meta $meta
    if (-not ($className -eq "Folder" -and -not $hasMetaSettings)) {
        Write-MetaJsonFile -Value $meta -Path $initMetaPath
    }

    if ($hasChildren) {
        $nameCounter = @{}

        foreach ($child in $children) {
            $base = Sanitize-Name -Name ([string]$child.name)
            if (-not $nameCounter.ContainsKey($base)) {
                $nameCounter[$base] = 0
            }
            $nameCounter[$base] += 1

            $stem = if ($nameCounter[$base] -eq 1) { $base } else { "$base`_$($nameCounter[$base])" }
            Emit-Node -Instance $child -ParentDir $dirPath -FsStem $stem
        }
    }
}
foreach ($service in $TargetServices) {
    if (-not $serviceRootByName.ContainsKey($service)) {
        throw "Snapshot missing root service instance: $service"
    }
    $serviceInstance = $serviceRootByName[$service]
    $servicePath = [string](Get-Field -Obj $serviceInstance -Name "path")

    $serviceDir = Join-Path $srcRoot (Sanitize-Name -Name $service)
    if (Test-Path -LiteralPath $serviceDir) {
        Remove-Item -LiteralPath $serviceDir -Recurse -Force
    }
    Ensure-Directory -Path $serviceDir

    $serviceKey = Get-InstanceKey -Instance $serviceInstance
    if (-not $script:VisitedInstanceKeys.ContainsKey($serviceKey)) {
        $script:VisitedInstanceKeys[$serviceKey] = $true
        Write-SyncInstanceProgress -InstancePath $servicePath -ClassName ([string]$serviceInstance.className)
    }
    $serviceMeta = Build-Meta -Instance $serviceInstance -ClassDefaultsByClass $classDefaultsByClass
    Write-MetaJsonFile -Value $serviceMeta -Path (Join-Path $serviceDir "init.meta.json")

    $children = Get-ChildrenForInstance -Instance $serviceInstance
    if ($children) {
        $nameCounter = @{}

        foreach ($child in $children) {
            $base = Sanitize-Name -Name ([string]$child.name)
            if (-not $nameCounter.ContainsKey($base)) {
                $nameCounter[$base] = 0
            }
            $nameCounter[$base] += 1
            $stem = if ($nameCounter[$base] -eq 1) { $base } else { "$base`_$($nameCounter[$base])" }
            Emit-Node -Instance $child -ParentDir $serviceDir -FsStem $stem
        }
    }
}

if (-not $NoProjectWrite) {
    $tree = [ordered]@{ '$className' = 'DataModel' }
    foreach ($service in $TargetServices) {
        $tree[$service] = [ordered]@{ '$path' = "src/$service" }
    }

    $project = [ordered]@{
        name = $projectName
        tree = $tree
    }

    $projectPath = Join-Path $ProjectRoot "default.project.generated.json"
    $projectJson = $project | ConvertTo-Json -Depth 20
    Write-Utf8File -Path $projectPath -Content $projectJson

    Write-Output "Imported snapshots into src tree and wrote default.project.generated.json"
} else {
    Write-Output "Imported snapshots into src tree"
}
