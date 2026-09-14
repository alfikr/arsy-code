$ErrorActionPreference = "Stop"

$Repository = "suiflex/arsy-code"
$Version = if ($env:ARSY_VERSION) { $env:ARSY_VERSION } else { "latest" }
if ($Version -notmatch '^(latest|v[0-9A-Za-z._-]+)$') {
    throw "Invalid ARSY_VERSION: $Version"
}

$Architecture = switch ($env:PROCESSOR_ARCHITECTURE) {
    "AMD64" { "x86_64" }
    default { throw "Unsupported architecture: $env:PROCESSOR_ARCHITECTURE" }
}

$Archive = "arsy-windows-$Architecture.zip"
$DownloadBase = if ($env:ARSY_DOWNLOAD_BASE) {
    $env:ARSY_DOWNLOAD_BASE.TrimEnd('/')
} elseif ($Version -eq "latest") {
    "https://github.com/$Repository/releases/latest/download"
} else {
    "https://github.com/$Repository/releases/download/$Version"
}

$TemporaryDirectory = Join-Path ([IO.Path]::GetTempPath()) ("arsy-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $TemporaryDirectory | Out-Null

try {
    $ArchivePath = Join-Path $TemporaryDirectory $Archive
    $ChecksumPath = "$ArchivePath.sha256"
    Invoke-WebRequest -UseBasicParsing -Uri "$DownloadBase/$Archive" -OutFile $ArchivePath
    Invoke-WebRequest -UseBasicParsing -Uri "$DownloadBase/$Archive.sha256" -OutFile $ChecksumPath

    # Checksum only: this is not the canonical verification path. The
    # canonical GitHub Release archive additionally carries a Sigstore
    # signature and bundle — see "Install and verify" in
    # docs/34-distribution.md. Verify those yourself for anything beyond a
    # quick local install.
    $ExpectedChecksum = (Get-Content -Raw $ChecksumPath).Trim().ToLowerInvariant()
    $ActualChecksum = (Get-FileHash -Algorithm SHA256 $ArchivePath).Hash.ToLowerInvariant()
    if ($ExpectedChecksum -ne $ActualChecksum) {
        throw "Checksum verification failed"
    }

    Expand-Archive -Path $ArchivePath -DestinationPath $TemporaryDirectory -Force
    $InstallDirectory = if ($env:ARSY_INSTALL_DIR) {
        $env:ARSY_INSTALL_DIR
    } else {
        Join-Path $env:LOCALAPPDATA "ArsyCode\bin"
    }
    New-Item -ItemType Directory -Force -Path $InstallDirectory | Out-Null
    $BinaryPath = Join-Path $InstallDirectory "arsy.exe"
    Copy-Item -Force (Join-Path $TemporaryDirectory "arsy.exe") $BinaryPath

    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $PathEntries = @($UserPath -split ';' | Where-Object { $_ })
    if ($InstallDirectory -notin $PathEntries) {
        $UpdatedPath = (@($PathEntries) + $InstallDirectory) -join ';'
        [Environment]::SetEnvironmentVariable("Path", $UpdatedPath, "User")
    }
    $env:Path = "$InstallDirectory;$env:Path"

    Write-Host ""
    Write-Host "ARSY CODE installed: $BinaryPath"
    Write-Host "Restart terminal, then run:"
    Write-Host "  arsy doctor"
}
finally {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $TemporaryDirectory
}
