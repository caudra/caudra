#Requires -Version 5.1
Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$Repo = "caudra/caudra"
$Binary = "caudra"
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

function Get-LatestTag {
    $headers = Get-GitHubHeaders
    try {
        $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -Headers $headers
    } catch {
        Write-Err "failed to determine latest release tag: $_"
    }
    $tag = $release.tag_name
    if (-not $tag) {
        Write-Err "failed to determine latest release tag"
    }
    return $tag
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

function Install-Payload([string]$Source, [string]$Licenses, [string]$Destination, [string]$LicenseDir) {
    $suffix = '.caudra-backup.' + [guid]::NewGuid().ToString('N')
    $bundleStage = Join-Path (Split-Path $LicenseDir -Parent) $suffix
    $binaryStage = Join-Path (Split-Path $Destination -Parent) $suffix
    $oldBundle = $false
    $newBundle = $false
    $oldBinary = $false
    $committed = $false
    $recoveryFailed = $false
    $createdStages = @()
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
                if ($committed -and (Test-Path -LiteralPath (Join-Path $stage 'previous'))) {
                    Write-Host "previous installation retained in $stage/previous"
                } elseif (Test-Path -LiteralPath $stage) {
                    Remove-Item -LiteralPath $stage -Recurse -Force
                }
            }
        }
    }
}

function Install-Caudra([string]$Tag) {
    $target = Get-Target
    if (-not $Tag) {
        $Tag = Get-LatestTag
    }

    $archiveName = "$Binary-$Tag-$target.zip"
    $url = "https://github.com/$Repo/releases/download/$Tag/$archiveName"
    $tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("caudra-install-" + [guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Path $tmp | Out-Null

    try {
        $zipPath = Join-Path $tmp $archiveName
        Write-Host "downloading $Binary $Tag for $target..."
        Invoke-WebRequest -Uri $url -OutFile $zipPath -Headers (Get-GitHubHeaders)

        Expand-Archive -Path $zipPath -DestinationPath $tmp -Force

        $exeName = "$Binary.exe"
        $src = Join-Path $tmp $exeName
        if (-not (Test-Path -LiteralPath $src)) {
            Write-Err "archive did not contain $exeName"
        }
        $licenses = Join-Path $tmp "licenses"
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

        Write-Host "$Binary $Tag installed to $dest"
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

$tag = if ($args.Count -ge 1) { $args[0] } else { $null }
Install-Caudra -Tag $tag
