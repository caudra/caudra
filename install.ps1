#Requires -Version 5.1
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$Repo = "caudra/caudra"
$Binary = "caudra"
$ReleaseResponseLimit = 4MB
$ChecksumLimit = 1MB
$ArchiveLimit = 2GB
$TransferBufferSize = 32KB
$ApiTimeoutMilliseconds = 30000
$AssetTimeoutMilliseconds = 300000
$AssetRedirectLimit = 3
$InstallDir = if ($env:CAUDRA_INSTALL_DIR) {
    $env:CAUDRA_INSTALL_DIR
} else {
    Join-Path $env:LOCALAPPDATA "caudra"
}

function Write-Err([string]$Message) {
    [Console]::Error.WriteLine("error: $Message")
    exit 1
}

function Get-GitHubHeaders {
    $headers = @{
        "User-Agent" = "caudra-install"
        "Accept"     = "application/vnd.github+json"
    }
    $token = $env:GITHUB_TOKEN
    if (-not $token) {
        $token = $env:GH_TOKEN
    }
    if ($token) {
        $headers["Authorization"] = "Bearer $token"
    }
    return $headers
}

function Get-Target {
    $arch = $env:PROCESSOR_ARCHITECTURE
    switch -Regex ($arch) {
        "^(AMD64|x86_64)$" { return "x86_64-pc-windows-msvc" }
        "^ARM64$" {
            # No native ARM64 release yet; x64 runs under emulation on Windows ARM.
            return "x86_64-pc-windows-msvc"
        }
        default { Write-Err "unsupported architecture: $arch" }
    }
}

function Get-SemVer([string]$Tag) {
    $pattern = '^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?(?:\+([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?\z'
    $match = [regex]::Match($Tag, $pattern)
    if (-not $match.Success) { return $null }
    $pre = $match.Groups[4].Value
    foreach ($identifier in ($pre -split '\.')) {
        if ($identifier -match '^0[0-9]+$') { return $null }
    }
    return @{
        Core = @($match.Groups[1].Value, $match.Groups[2].Value, $match.Groups[3].Value)
        Pre = $pre
    }
}

function Compare-NumericIdentifier([string]$Left, [string]$Right) {
    if ($Left.Length -ne $Right.Length) { return $Left.Length.CompareTo($Right.Length) }
    return [string]::CompareOrdinal($Left, $Right)
}

function Compare-SemVer($Left, $Right) {
    for ($i = 0; $i -lt 3; $i++) {
        $comparison = Compare-NumericIdentifier $Left.Core[$i] $Right.Core[$i]
        if ($comparison -ne 0) { return $comparison }
    }
    if (-not $Left.Pre -or -not $Right.Pre) {
        if ($Left.Pre -eq $Right.Pre) { return 0 }
        if (-not $Left.Pre) { return 1 }
        return -1
    }
    $a = $Left.Pre -split '\.'
    $b = $Right.Pre -split '\.'
    for ($i = 0; $i -lt [Math]::Min($a.Count, $b.Count); $i++) {
        if ($a[$i] -ceq $b[$i]) { continue }
        $aNumeric = $a[$i] -match '^[0-9]+$'
        $bNumeric = $b[$i] -match '^[0-9]+$'
        if ($aNumeric -and $bNumeric) { return Compare-NumericIdentifier $a[$i] $b[$i] }
        if ($aNumeric) { return -1 }
        if ($bNumeric) { return 1 }
        return [string]::CompareOrdinal($a[$i], $b[$i])
    }
    return $a.Count.CompareTo($b.Count)
}

function Assert-JsonValue([string[]]$Tokens, [ref]$Position, [int]$Depth) {
    if ($Depth -gt 32 -or $Position.Value -ge $Tokens.Count) { throw 'invalid JSON nesting or truncated value' }
    $token = $Tokens[$Position.Value++]
    if ($token -eq '{' -or $token -eq '[') {
        $object = $token -eq '{'
        $end = if ($object) { '}' } else { ']' }
        $keys = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
        if ($Position.Value -lt $Tokens.Count -and $Tokens[$Position.Value] -eq $end) { $Position.Value++; return }
        while ($Position.Value -lt $Tokens.Count) {
            if ($object) {
                $key = $Tokens[$Position.Value++]
                if (-not $key.StartsWith('"') -or -not $keys.Add([regex]::Unescape($key.Substring(1, $key.Length - 2)))) {
                    throw 'invalid or duplicate JSON member'
                }
                if ($Position.Value -ge $Tokens.Count -or $Tokens[$Position.Value++] -ne ':') { throw 'invalid JSON object' }
            }
            Assert-JsonValue $Tokens $Position ($Depth + 1)
            if ($Position.Value -ge $Tokens.Count) { throw 'truncated JSON container' }
            $separator = $Tokens[$Position.Value++]
            if ($separator -eq $end) { return }
            if ($separator -ne ',') { throw 'invalid JSON separator' }
        }
        throw 'truncated JSON container'
    }
    if ($token -in @('}', ']', ',', ':')) { throw 'invalid JSON value' }
}

function Assert-ReleaseJson([string]$Content) {
    $pattern = '\G[ \t\r\n]*("(?:[^"\\\x00-\x1f]|\\(?:["\\/bfnrt]|u[0-9a-fA-F]{4}))*"|-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?|true|false|null|[\[\]{},:])'
    $matches = [regex]::Matches($Content, $pattern)
    $tokens = @($matches | ForEach-Object { $_.Groups[1].Value })
    $consumed = if ($matches.Count) { $matches[$matches.Count - 1].Index + $matches[$matches.Count - 1].Length } else { 0 }
    if ($Content.Substring($consumed) -notmatch '^[ \t\r\n]*$') { throw 'invalid JSON token' }
    $position = 0
    Assert-JsonValue $tokens ([ref]$position) 0
    if ($position -ne $tokens.Count) { throw 'trailing JSON data' }
}

function Get-ReleaseResponse([string]$Uri, [bool]$List) {
    $body = [IO.MemoryStream]::new()
    try {
        Receive-ReleaseData $Uri $body $ReleaseResponseLimit $true
        $content = [Text.UTF8Encoding]::new($false, $true).GetString($body.ToArray())
    } finally { $body.Dispose() }
    Assert-ReleaseJson $content
    if (($List -and -not $content.TrimStart().StartsWith('[')) -or
        (-not $List -and -not $content.TrimStart().StartsWith('{'))) { throw 'invalid release response' }
    $parsed = ConvertFrom-Json -InputObject ('{"release_data":' + $content + '}')
    if ($List) { return @{ Data = $parsed.release_data } }
    return @{ Data = @($parsed.release_data) }
}

function Get-ReleaseCandidate($Release, [string]$Target) {
    if ($null -eq $Release -or $Release.tag_name -isnot [string] -or
        $Release.draft -isnot [bool] -or $Release.prerelease -isnot [bool]) { throw 'invalid release metadata' }
    $version = Get-SemVer $Release.tag_name
    if ($Release.draft -or -not $version) { return $null }
    if ($Release.published_at -isnot [datetime] -and
        ($Release.published_at -isnot [string] -or
        $Release.published_at -notmatch '^[0-9]{4}-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z$')) { return $null }
    if ($Release.prerelease -ne [bool]$version.Pre) { return $null }
    $archive = "$Binary-$($Release.tag_name)-$Target.zip"
    $base = "https://github.com/$Repo/releases/download/$($Release.tag_name)/"
    $usable = $Release.assets -is [array]
    foreach ($name in @($archive, 'sha256sums.txt', 'install.ps1')) {
        $assets = @($Release.assets | Where-Object { $_.name -ceq $name })
        if ($assets.Count -ne 1) { $usable = $false; continue }
        $asset = $assets[0]
        if ($asset.state -cne 'uploaded' -or $asset.size -is [string] -or $asset.size -is [bool] -or $asset.size -le 0 -or
            $asset.browser_download_url -cne ($base + $name)) { $usable = $false }
    }
    return @{ Tag = $Release.tag_name; Version = $version; Usable = $usable }
}

function Resolve-ReleaseTag([string]$Tag, [string]$Channel, [string]$Target) {
    $releases = @()
    if ($Tag) {
        if (-not (Get-SemVer $Tag)) { throw 'expected a v-prefixed SemVer tag' }
        $response = Get-ReleaseResponse "https://api.github.com/repos/$Repo/releases/tags/$Tag" $false
        if ($response.Data.Count -ne 1 -or $response.Data[0].tag_name -cne $Tag) { throw 'release tag mismatch' }
        $releases = $response.Data
    } else {
        for ($page = 1; $page -le 10; $page++) {
            $response = Get-ReleaseResponse "https://api.github.com/repos/$Repo/releases?per_page=100&page=$page" $true
            if ($response.Data.Count -gt 100) { throw 'invalid release list' }
            if ($response.Data.Count -eq 0) { break }
            $releases += $response.Data
        }
        if ($page -gt 10) { throw 'release discovery exceeded 10 pages; refusing incomplete selection' }
    }
    $best = $null
    $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    foreach ($release in $releases) {
        $candidate = Get-ReleaseCandidate $release $Target
        if (-not $candidate) { continue }
        if (-not $seen.Add($candidate.Tag)) { throw 'duplicate release across pages' }
        $preview = [bool]$candidate.Version.Pre
        if ($Channel -eq 'stable' -and $preview) { continue }
        if (-not $best -or ($Channel -eq 'auto' -and $best.Version.Pre -and -not $preview) -or
            (($Channel -eq 'preview' -or [bool]$best.Version.Pre -eq $preview) -and
            (Compare-SemVer $candidate.Version $best.Version) -gt 0)) {
            $best = $candidate
        }
    }
    if (-not $best) { throw 'no published release for requested channel/tag' }
    if (-not $best.Usable) { throw 'selected release lacks required uploaded assets' }
    return $best.Tag
}

function Open-ReleaseResponse([string]$Uri, [hashtable]$Headers, [int]$TimeoutMilliseconds) {
    $request = [Net.HttpWebRequest]::Create($Uri)
    $request.AllowAutoRedirect = $false
    $request.Timeout = $TimeoutMilliseconds
    $request.ReadWriteTimeout = $TimeoutMilliseconds
    foreach ($name in $Headers.Keys) {
        switch ($name) {
            'User-Agent' { $request.UserAgent = $Headers[$name] }
            'Accept' { $request.Accept = $Headers[$name] }
            default { $request.Headers[$name] = $Headers[$name] }
        }
    }
    return $request.GetResponse()
}

function Receive-ReleaseData([string]$Uri, [IO.Stream]$Output, [long]$MaxBytes, [bool]$Api) {
    $headers = @{}
    $timeout = $AssetTimeoutMilliseconds
    $redirectLimit = $AssetRedirectLimit
    if ($Api) {
        if (-not $Uri.StartsWith("https://api.github.com/repos/$Repo/releases", [StringComparison]::Ordinal)) {
            throw 'refusing non-GitHub API origin'
        }
        $headers = Get-GitHubHeaders
        $timeout = $ApiTimeoutMilliseconds
        $redirectLimit = 0
    }
    $clock = [Diagnostics.Stopwatch]::StartNew()
    for ($redirect = 0; $redirect -le $redirectLimit; $redirect++) {
        if (([uri]$Uri).Scheme -cne 'https' -or ([uri]$Uri).UserInfo) {
            throw 'refusing non-HTTPS release origin'
        }
        $remaining = $timeout - $clock.ElapsedMilliseconds
        if ($remaining -le 0) { throw 'release request timed out' }
        $response = Open-ReleaseResponse $Uri $headers $remaining
        try {
            if ([int]$response.StatusCode -in @(301, 302, 303, 307, 308)) {
                if ($redirect -eq $redirectLimit) { throw 'release redirect limit exceeded' }
                if (-not $response.Headers['Location']) { throw 'release redirect missing Location' }
                $Uri = [uri]::new([uri]$Uri, $response.Headers['Location']).AbsoluteUri
                continue
            }
            if ([int]$response.StatusCode -ne 200) { throw "release request returned HTTP $([int]$response.StatusCode)" }
            if ($response.ContentLength -gt $MaxBytes) { throw 'release response too large' }
            $inputStream = $response.GetResponseStream()
            try {
                $buffer = [byte[]]::new($TransferBufferSize)
                $total = 0L
                while ($true) {
                    $remaining = $timeout - $clock.ElapsedMilliseconds
                    if ($remaining -le 0) { throw 'release request timed out' }
                    if ($inputStream.CanTimeout) { $inputStream.ReadTimeout = $remaining }
                    $count = $inputStream.Read($buffer, 0, [int][Math]::Min([long]$buffer.Length, $MaxBytes - $total + 1))
                    if ($count -eq 0) { break }
                    $total += $count
                    if ($total -gt $MaxBytes) { throw 'release response too large' }
                    $Output.Write($buffer, 0, $count)
                }
                if ($response.ContentLength -ge 0 -and $total -ne $response.ContentLength) { throw 'truncated release response' }
            } finally { $inputStream.Dispose() }
            return
        } finally { $response.Dispose() }
    }
}

function Save-ReleaseAsset([string]$Uri, [string]$Destination, [long]$MaxBytes = $ArchiveLimit) {
    $file = [IO.File]::Create($Destination)
    try { Receive-ReleaseData $Uri $file $MaxBytes $false } finally { $file.Dispose() }
}

function Assert-ArchiveChecksum([string]$Manifest, [string]$Archive) {
    $name = [IO.Path]::GetFileName($Archive)
    $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    $expected = $null
    foreach ($line in [IO.File]::ReadAllLines($Manifest)) {
        $match = [regex]::Match($line, '^([0-9a-fA-F]{64}) [ *]([A-Za-z0-9][A-Za-z0-9._+-]*)$')
        if (-not $match.Success -or -not $seen.Add($match.Groups[2].Value)) {
            throw 'invalid or duplicate archive checksum'
        }
        if ($match.Groups[2].Value -ceq $name) { $expected = $match.Groups[1].Value }
    }
    if (-not $expected) { throw 'missing archive checksum' }
    if ((Get-FileHash -LiteralPath $Archive -Algorithm SHA256).Hash -ine $expected) {
        throw 'archive SHA-256 checksum mismatch'
    }
}

function Assert-RegularItem($Item) {
    if (($Item.Attributes -band ([IO.FileAttributes]::ReparsePoint -bor [IO.FileAttributes]::Device)) -ne 0 -or
        ($Item -isnot [IO.FileInfo] -and $Item -isnot [IO.DirectoryInfo])) {
        throw "refusing reparse point or special file: $($Item.FullName)"
    }
}

function Assert-SafePath([string]$Path) {
    while ($Path) {
        $itemError = @()
        $item = Get-Item -LiteralPath $Path -Force -ErrorAction SilentlyContinue -ErrorVariable itemError
        foreach ($errorRecord in $itemError) {
            if ($errorRecord.CategoryInfo.Category -ne 'ObjectNotFound') {
                throw $errorRecord
            }
        }
        if ($item) { Assert-RegularItem $item }
        $Path = Split-Path -Path $Path -Parent
    }
}

function Assert-RegularTree([string]$Path) {
    $item = Get-Item -LiteralPath $Path -Force
    Assert-RegularItem $item
    if ($item.PSIsContainer) {
        foreach ($child in (Get-ChildItem -LiteralPath $Path -Force)) {
            Assert-RegularTree $child.FullName
        }
    }
}

function Assert-ArchiveEntries([string]$Path) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zip = [IO.Compression.ZipFile]::OpenRead($Path)
    try {
        if ($zip.Entries.Count -gt 100000) { throw 'archive inventory too large' }
        $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
        $size = 0L
        foreach ($entry in $zip.Entries) {
            $name = $entry.FullName.TrimEnd('/')
            if ($name -cmatch '[\x00-\x1f\x7f\\:]|(^|/)\.\.?($|/)|//|[. ]($|/)' -or
                ($name -cnotin @('caudra.exe', 'LICENSE', 'NOTICE.md', 'licenses', 'THIRD_PARTY_LICENSES') -and
                    -not $name.StartsWith('licenses/') -and -not $name.StartsWith('THIRD_PARTY_LICENSES/')) -or
                -not $seen.Add($name)) { throw 'unsafe or duplicate archive path' }
            foreach ($part in ($name -split '/')) {
                if ($part -match '^(CON|PRN|AUX|NUL|COM[0-9]|LPT[0-9])([.]|$)') { throw 'unsafe archive device path' }
            }
            $kind = ($entry.ExternalAttributes -shr 16) -band 0xF000
            if ($kind -notin @(0, 0x8000, 0x4000) -or ($entry.ExternalAttributes -band 0x440) -ne 0 -or
                ($kind -eq 0x4000 -and -not $entry.FullName.EndsWith('/'))) { throw 'refusing archive link or special file' }
            $size += $entry.Length
            if ($size -gt $ArchiveLimit) { throw 'expanded archive too large' }
        }
    } finally { $zip.Dispose() }
}

function Read-BundleManifest([string]$Path) {
    if ((Get-Item -LiteralPath $Path).Length -gt 16MB) { throw 'bundle manifest too large' }
    $text = [IO.File]::ReadAllText($Path)
    $tokens = [regex]::Matches($text, '"(?:[^"\\\x00-\x1f]|\\(?:["\\/bfnrt]|u[0-9a-fA-F]{4}))*"|[{}\[\]:,]|true|false|null|-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?')
    $stack = [Collections.Generic.Stack[object]]::new()
    $end = 0
    for ($i = 0; $i -lt $tokens.Count; $i++) {
        $token = $tokens[$i]
        if ($text.Substring($end, $token.Index - $end) -notmatch '^\s*$') { throw 'invalid bundle JSON' }
        $end = $token.Index + $token.Length
        if ($token.Value -in @('{', '[')) {
            $stack.Push([Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase))
            if ($stack.Count -gt 32) { throw 'bundle nesting limit exceeded' }
        } elseif ($token.Value -in @('}', ']')) {
            if ($stack.Count -eq 0) { throw 'invalid bundle JSON' }
            $null = $stack.Pop()
        } elseif ($token.Value.StartsWith('"') -and $i + 1 -lt $tokens.Count -and $tokens[$i + 1].Value -eq ':') {
            $key = ConvertFrom-Json -InputObject $token.Value
            if ($stack.Count -eq 0 -or -not $stack.Peek().Add($key)) { throw 'duplicate bundle JSON member' }
        }
    }
    if ($stack.Count -ne 0 -or $text.Substring($end) -notmatch '^\s*$') { throw 'invalid bundle JSON' }
    return ConvertFrom-Json -InputObject $text
}

function Assert-LicenseBundle([string]$Path, [string]$Target) {
    Assert-RegularTree $Path
    foreach ($name in @('manifest.json', 'NOTICE', 'ATTRIBUTION.txt')) {
        $file = Join-Path $Path $name
        if (-not (Test-Path -LiteralPath $file -PathType Leaf) -or (Get-Item -LiteralPath $file).Length -eq 0) {
            throw "archive did not contain licenses/$name"
        }
    }
    $manifest = Read-BundleManifest (Join-Path $Path 'manifest.json')
    if ($manifest.schema_version -isnot [int] -and $manifest.schema_version -isnot [long]) {
        throw 'invalid bundle schema version'
    }
    $reader = [IO.File]::OpenText((Join-Path $Path 'ATTRIBUTION.txt'))
    try { $marker = $reader.ReadLine() } finally { $reader.Dispose() }
    $layout = $manifest.PSObject.Properties['layout']
    if ($manifest.schema_version -ceq 1 -and (-not $layout -or $layout.Value -ceq 'expanded') -and
        $marker -cnotlike 'CAUDRA-ATTRIBUTION *') { return }
    if ($manifest.schema_version -cne 2 -or -not $layout -or $layout.Value -isnot [string] -or $layout.Value -cne 'compact' -or
        $marker -cne 'CAUDRA-ATTRIBUTION compact-v2' -or $manifest.target -cne $Target) {
        throw 'unsupported or inconsistent bundle layout'
    }
    $names = @('LICENSE', 'NOTICE', 'THIRD_PARTY_NOTICES.txt', 'ATTRIBUTION.txt', 'attribution.tar.gz')
    $children = @(Get-ChildItem -LiteralPath $Path -Force)
    if ($children.Count -ne 6 -or @($children | Where-Object { $_.PSIsContainer }).Count -ne 0 -or
        $manifest.files -isnot [array] -or @($manifest.files).Count -ne 5) { throw 'compact bundle must contain exactly six regular files' }
    $seen = [Collections.Generic.HashSet[string]]::new([StringComparer]::Ordinal)
    foreach ($file in $manifest.files) {
        if ($file.path -cnotin $names -or -not $seen.Add($file.path) -or $file.sha256 -cnotmatch '^[0-9a-f]{64}$') {
            throw 'invalid compact file inventory'
        }
        $source = Join-Path $Path $file.path
        if (-not (Test-Path -LiteralPath $source -PathType Leaf) -or (Get-Item -LiteralPath $source).Length -eq 0 -or
            (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash -ine $file.sha256) { throw 'compact file checksum mismatch' }
    }
}

function Remove-PreviousBackupPairs([string]$Destination, [string]$LicenseDir, [string]$Keep, [string]$Owner) {
    foreach ($candidate in (Get-ChildItem -LiteralPath (Split-Path $Destination -Parent) -Force)) {
        if ($candidate.Name -cnotmatch '^\.caudra-backup\.[A-Za-z0-9]+$' -or $candidate.FullName -eq $Keep) { continue }
        $bundle = Join-Path (Split-Path $LicenseDir -Parent) $candidate.Name
        try {
            foreach ($stage in @($candidate.FullName, $bundle)) {
                Assert-SafePath $stage
                Assert-RegularTree $stage
                if (@(Get-ChildItem -LiteralPath $stage -Force).Count -ne 2) { throw 'unrecognized backup contents' }
                $marker = Join-Path $stage 'managed-pair'
                if (-not (Test-Path -LiteralPath $marker -PathType Leaf) -or (Get-Item -LiteralPath $marker).Length -gt 8192 -or
                    [IO.File]::ReadAllText($marker) -cne $Owner) { throw 'unrecognized backup ownership' }
            }
            if (-not (Test-Path -LiteralPath (Join-Path $candidate.FullName 'previous') -PathType Leaf) -or
                -not (Test-Path -LiteralPath (Join-Path $bundle 'previous') -PathType Container)) { continue }
            Assert-LicenseBundle (Join-Path $bundle 'previous') (Get-Target)
            Remove-Item -LiteralPath $bundle -Recurse -Force
            Remove-Item -LiteralPath $candidate.FullName -Recurse -Force
        } catch { Write-Verbose "Leaving unrecognized or inaccessible backup: $_" }
    }
}

function Test-SameTree([string]$Left, [string]$Right) {
    $a = Get-Item -LiteralPath $Left -Force
    $b = Get-Item -LiteralPath $Right -Force
    if ($a.PSIsContainer -ne $b.PSIsContainer) { return $false }
    if (-not $a.PSIsContainer) {
        return $a.Length -eq $b.Length -and
            (Get-FileHash -LiteralPath $Left -Algorithm SHA256).Hash -ceq (Get-FileHash -LiteralPath $Right -Algorithm SHA256).Hash
    }
    $children = @(Get-ChildItem -LiteralPath $Left -Force)
    if ($children.Count -ne @(Get-ChildItem -LiteralPath $Right -Force).Count) { return $false }
    foreach ($child in $children) {
        $other = Join-Path $Right $child.Name
        if (-not (Test-Path -LiteralPath $other) -or -not (Test-SameTree $child.FullName $other)) { return $false }
    }
    return $true
}

function Test-PreviousSnapshot([string]$BinaryStage, [string]$BundleStage, [bool]$OldBundle) {
    $snapshot = $env:CAUDRA_UPDATE_SNAPSHOT
    if (-not $snapshot) { return $false }
    try {
        Assert-SafePath $snapshot
        Assert-RegularTree $snapshot
        if (@(Get-ChildItem -LiteralPath $snapshot -Force).Count -ne 2) { return $false }
        if (-not (Test-SameTree (Join-Path $snapshot 'binary') (Join-Path $BinaryStage 'previous'))) { return $false }
        if ($OldBundle) {
            Assert-LicenseBundle (Join-Path $snapshot 'licenses') (Get-Target)
            return Test-SameTree (Join-Path $snapshot 'licenses') (Join-Path $BundleStage 'previous')
        }
        return [IO.File]::ReadAllText((Join-Path $snapshot 'no-license-bundle')) -ceq "The previous installation did not contain a license bundle.`n"
    } catch { return $false }
}

function Install-Payload([string]$Source, [string]$Licenses, [string]$Destination, [string]$LicenseDir) {
    $suffix = '.caudra-backup.' + [guid]::NewGuid().ToString('N')
    $bundleStage = Join-Path (Split-Path $LicenseDir -Parent) $suffix
    $binaryStage = Join-Path (Split-Path $Destination -Parent) $suffix
    $oldBundle = $false
    $newBundle = $false
    $oldBinary = $false
    $committed = $false
    $retainPrevious = $true
    $recoveryFailed = $false
    $createdStages = @()
    $previousComplete = $false
    if ((Test-Path -LiteralPath $Destination -PathType Leaf) -and (Test-Path -LiteralPath $LicenseDir -PathType Container)) {
        try {
            Assert-LicenseBundle $LicenseDir (Get-Target)
            $previousComplete = $true
        } catch { Write-Verbose "Previous bundle is not eligible for automatic backup rotation: $_" }
    }
    try {
        New-Item -ItemType Directory -Path $bundleStage | Out-Null
        $createdStages += $bundleStage
        New-Item -ItemType Directory -Path $binaryStage | Out-Null
        $createdStages += $binaryStage
        Copy-Item -LiteralPath $Licenses -Destination (Join-Path $bundleStage 'new') -Recurse
        Copy-Item -LiteralPath $Source -Destination (Join-Path $binaryStage 'new')
        Assert-RegularTree (Join-Path $bundleStage 'new')
        Assert-RegularTree (Join-Path $binaryStage 'new')
        Assert-SafePath $Destination
        Assert-SafePath $LicenseDir
        if (Test-Path -LiteralPath $LicenseDir) {
            Assert-RegularTree $LicenseDir
            Move-Item -LiteralPath $LicenseDir -Destination (Join-Path $bundleStage 'previous')
            $oldBundle = $true
        }
        Move-Item -LiteralPath (Join-Path $bundleStage 'new') -Destination $LicenseDir
        $newBundle = $true
        if (Test-Path -LiteralPath $Destination) {
            Move-Item -LiteralPath $Destination -Destination (Join-Path $binaryStage 'previous')
            $oldBinary = $true
        }
        Move-Item -LiteralPath (Join-Path $binaryStage 'new') -Destination $Destination
        $committed = $true
        $owner = "caudra-install-pair-v1`n$Destination`n$LicenseDir`n"
        if (Test-PreviousSnapshot $binaryStage $bundleStage $oldBundle) {
            $retainPrevious = $false
            Remove-PreviousBackupPairs $Destination $LicenseDir $binaryStage $owner
        } elseif ($previousComplete) {
            foreach ($stage in @($binaryStage, $bundleStage)) {
                [IO.File]::WriteAllText((Join-Path $stage 'managed-pair'), $owner)
            }
            Remove-PreviousBackupPairs $Destination $LicenseDir $binaryStage $owner
        }
    } finally {
        if (-not $committed) {
            try {
                if ($oldBinary) {
                    if (Test-Path -LiteralPath $Destination) { throw "binary destination occupied during recovery" }
                    Move-Item -LiteralPath (Join-Path $binaryStage 'previous') -Destination $Destination
                }
                if ($newBundle) {
                    Move-Item -LiteralPath $LicenseDir -Destination (Join-Path $bundleStage 'new')
                }
                if ($oldBundle) {
                    if (Test-Path -LiteralPath $LicenseDir) { throw "license destination occupied during recovery" }
                    Move-Item -LiteralPath (Join-Path $bundleStage 'previous') -Destination $LicenseDir
                }
            } catch {
                $recoveryFailed = $true
                Write-Warning "recovery failed; retained installation data in $bundleStage and ${binaryStage}: $_"
            }
        }
        if (-not $recoveryFailed) {
            foreach ($stage in $createdStages) {
                if ($committed -and $retainPrevious -and (Test-Path -LiteralPath (Join-Path $stage 'previous'))) {
                    Write-Host "previous installation retained in $stage/previous"
                } elseif (Test-Path -LiteralPath $stage) {
                    Remove-Item -LiteralPath $stage -Recurse -Force
                }
            }
        }
    }
}

function Install-Caudra([string]$Tag, [string]$Channel) {
    $target = Get-Target
    $Tag = Resolve-ReleaseTag $Tag $Channel $target
    $label = if ((Get-SemVer $Tag).Pre) { ' (Preview)' } else { '' }

    $archiveName = "$Binary-$Tag-$target.zip"
    $url = "https://github.com/$Repo/releases/download/$Tag/$archiveName"
    $tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("caudra-install-" + [guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Path $tmp | Out-Null

    try {
        $zipPath = Join-Path $tmp $archiveName
        Write-Host "downloading $Binary $Tag$label for $target..."
        Save-ReleaseAsset $url $zipPath
        $sumsPath = Join-Path $tmp 'sha256sums.txt'
        Save-ReleaseAsset "https://github.com/$Repo/releases/download/$Tag/sha256sums.txt" $sumsPath $ChecksumLimit
        Assert-ArchiveChecksum $sumsPath $zipPath

        Assert-ArchiveEntries $zipPath
        $payload = Join-Path $tmp 'payload'
        Expand-Archive -Path $zipPath -DestinationPath $payload -Force

        $exeName = "$Binary.exe"
        $src = Join-Path $payload $exeName
        if (-not (Test-Path -LiteralPath $src)) {
            Write-Err "archive did not contain $exeName"
        }
        $licenses = Join-Path $payload "licenses"
        $manifest = Join-Path $licenses "manifest.json"
        if (-not (Test-Path -LiteralPath $manifest -PathType Leaf) -or (Get-Item -LiteralPath $manifest).Length -eq 0) {
            Write-Err "archive did not contain licenses/manifest.json; legacy archives without a license bundle are not supported"
        }
        $attribution = Join-Path $licenses "ATTRIBUTION.txt"
        if (-not (Test-Path -LiteralPath $attribution -PathType Leaf) -or (Get-Item -LiteralPath $attribution).Length -eq 0) {
            Write-Err "archive did not contain licenses/ATTRIBUTION.txt"
        }
        Assert-RegularTree $src
        Assert-RegularTree $licenses
        Assert-LicenseBundle $licenses $target
        if (-not (Test-Path -LiteralPath $src -PathType Leaf)) {
            throw "binary source is not a regular file"
        }

        $InstallDir = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($InstallDir)
        Assert-SafePath $InstallDir
        $licenseDir = [IO.Path]::GetFullPath((Join-Path $InstallDir "../share/licenses/caudra"))
        Assert-SafePath $licenseDir
        $dest = Join-Path $InstallDir $exeName
        Assert-SafePath $dest
        if ((Test-Path -LiteralPath $dest) -and -not (Test-Path -LiteralPath $dest -PathType Leaf)) {
            throw "binary destination is not a regular file"
        }
        if ((Test-Path -LiteralPath $licenseDir) -and -not (Test-Path -LiteralPath $licenseDir -PathType Container)) {
            throw "license destination is not a directory"
        }
        if (-not (Test-Path -LiteralPath $InstallDir)) {
            New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
        }

        try {
            New-Item -ItemType Directory -Path (Split-Path $licenseDir -Parent) -Force | Out-Null
            Install-Payload -Source $src -Licenses $licenses -Destination $dest -LicenseDir $licenseDir
        } catch {
            Write-Err "failed to install binary to $dest or licenses to $licenseDir (try running as Administrator or set CAUDRA_INSTALL_DIR): $_"
        }

        Write-Host "$Binary $Tag$label installed to $dest"
        Write-Host "licenses installed to $licenseDir"
        Add-ToUserPath -Dir $InstallDir
    } finally {
        Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Add-ToUserPath([string]$Dir) {
    $sep = [IO.Path]::PathSeparator
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($null -eq $userPath) {
        $userPath = ""
    }
    $entries = $userPath -split [regex]::Escape($sep) | Where-Object { $_ -ne "" }
    $already = $entries | Where-Object { $_.TrimEnd('\') -ieq $Dir.TrimEnd('\') }
    if ($already) {
        return
    }

    $newPath = if ($userPath.Trim()) { "$userPath$sep$Dir" } else { $Dir }
    [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
    $env:Path = "$env:Path$sep$Dir"
    Write-Host "added $Dir to user PATH (restart terminal if caudra is not found)"
}

function Invoke-CaudraInstaller([string[]]$Arguments) {
    $tag = ''
    $channel = 'auto'
    for ($i = 0; $i -lt $Arguments.Count; $i++) {
        switch -CaseSensitive ($Arguments[$i]) {
            '--channel' {
                if ($channel -ne 'auto' -or ++$i -ge $Arguments.Count -or $Arguments[$i] -cnotin @('stable', 'preview')) {
                    throw 'usage: install.ps1 [vVERSION | --channel stable|preview]'
                }
                $channel = $Arguments[$i]
            }
            { $_ -in @('--help', '-h') } { Write-Host 'usage: install.ps1 [vVERSION | --channel stable|preview]'; return }
            default {
                if ($tag -or $Arguments[$i].StartsWith('-')) { throw "unexpected argument: $($Arguments[$i])" }
                $tag = $Arguments[$i]
            }
        }
    }
    if ($tag -and $channel -ne 'auto') { throw 'a release tag cannot be combined with --channel' }
    Install-Caudra -Tag $tag -Channel $channel
}

Invoke-CaudraInstaller $args
