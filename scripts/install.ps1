$ErrorActionPreference = "Stop"

[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12 -bor [Net.ServicePointManager]::SecurityProtocol

$Repo = if ($env:LOOM_REPO) { $env:LOOM_REPO } else { "Wick-Lim/loom" }
$InstallDir = if ($env:LOOM_INSTALL_DIR) {
    $env:LOOM_INSTALL_DIR
} else {
    Join-Path $env:LOCALAPPDATA "Programs\loom-vcs\bin"
}
$Version = if ($env:LOOM_VERSION) { $env:LOOM_VERSION.Trim() } else { "" }
$NoModifyPath = $env:LOOM_NO_MODIFY_PATH -in @("1", "true", "TRUE", "yes")

function Get-TargetTriple {
    $archStr = $null
    try {
        $arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
        $archStr = $arch.ToString()
    } catch {
        $archStr = $env:PROCESSOR_ARCHITECTURE
    }

    switch -Regex ($archStr) {
        "^(X64|AMD64)$" { return "x86_64-pc-windows-msvc" }
        "^(Arm64|ARM64)$" { return "aarch64-pc-windows-msvc" }
        default { throw "Unsupported Windows architecture: $archStr" }
    }
}

$target = Get-TargetTriple

if ([string]::IsNullOrWhiteSpace($Version)) {
    Write-Host "Resolving latest loom-vcs release from https://github.com/$Repo..."
    $release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" -Headers @{ "User-Agent" = "loom-vcs-installer" }
    $tag = $release.tag_name
    if ([string]::IsNullOrWhiteSpace($tag)) {
        throw "Failed to resolve latest release tag from GitHub API."
    }
} else {
    $tag = if ($Version.StartsWith("v")) { $Version } else { "v$Version" }
}

$archive = "loom-vcs-$tag-$target.zip"
$baseUrl = "https://github.com/$Repo/releases/download/$tag"
$tempDir = Join-Path ([System.IO.Path]::GetTempPath()) ("loom-install-" + [Guid]::NewGuid().ToString("N"))

try {
    New-Item -ItemType Directory -Path $tempDir -Force | Out-Null
    $zipPath = Join-Path $tempDir $archive
    $sumsPath = Join-Path $tempDir "SHA256SUMS.txt"
    $extractDir = Join-Path $tempDir "extract"

    Write-Host "Downloading $archive..."
    Invoke-WebRequest -Uri "$baseUrl/$archive" -OutFile $zipPath -UseBasicParsing
    Invoke-WebRequest -Uri "$baseUrl/SHA256SUMS.txt" -OutFile $sumsPath -UseBasicParsing

    $expectedSha = $null
    foreach ($line in (Get-Content -Path $sumsPath)) {
        if ($line -match '^([a-fA-F0-9]{64})\s+\*?(.+)$') {
            $name = ($Matches[2].Trim() -replace '^\./', '')
            if ($name -eq $archive) {
                $expectedSha = $Matches[1].ToLowerInvariant()
                break
            }
        }
    }
    if (-not $expectedSha) {
        throw "Checksum entry for $archive not found in SHA256SUMS.txt"
    }

    $actualSha = (Get-FileHash -Path $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualSha -ne $expectedSha) {
        throw "SHA256 checksum mismatch for $archive (expected $expectedSha, got $actualSha)"
    }

    Expand-Archive -Path $zipPath -DestinationPath $extractDir -Force
    $srcExe = Join-Path $extractDir "loom.exe"
    if (-not (Test-Path $srcExe)) {
        throw "Extracted archive did not contain loom.exe"
    }

    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    $destExe = Join-Path $InstallDir "loom.exe"
    Copy-Item -Path $srcExe -Destination $destExe -Force

    Write-Host "Installed loom-vcs ($tag) to $destExe"
    & $destExe --version

    if (-not $NoModifyPath) {
        $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
        $entries = @()
        if ($userPath) {
            $entries = $userPath.Split(";") | Where-Object { $_ -ne "" }
        }
        $alreadyPresent = $entries | Where-Object { $_.TrimEnd("\") -ieq $InstallDir.TrimEnd("\") }
        if (-not $alreadyPresent) {
            $newUserPath = if ($entries.Count -gt 0) { ($entries + $InstallDir) -join ";" } else { $InstallDir }
            [Environment]::SetEnvironmentVariable("Path", $newUserPath, "User")
            if (-not (($env:Path.Split(";") | Where-Object { $_.TrimEnd("\") -ieq $InstallDir.TrimEnd("\") }))) {
                $env:Path = "$env:Path;$InstallDir"
            }
            Write-Host "Added $InstallDir to user PATH. Restart your terminal if 'loom' is not recognized immediately."
        }
    }
} finally {
    if (Test-Path $tempDir) {
        Remove-Item -Path $tempDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}
