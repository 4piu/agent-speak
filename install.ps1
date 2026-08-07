[CmdletBinding()]
param(
    [string]$Version = $env:VERSION,
    [string]$InstallDir = $env:INSTALL_DIR,
    [string]$ReleaseBaseUrl = $env:RELEASE_BASE_URL,
    [switch]$Uninstall,
    [switch]$Purge
)

$ErrorActionPreference = "Stop"
$Repository = "4piu/agent-speak"
$ArchivePrefix = "agent-speak"
$Programs = @("agent-speak.exe")
$ProviderSlug = ""

if ([string]::IsNullOrWhiteSpace($InstallDir)) {
    if ([string]::IsNullOrWhiteSpace($env:LOCALAPPDATA)) {
        throw "LOCALAPPDATA is not set"
    }
    $InstallDir = Join-Path $env:LOCALAPPDATA "Programs\UtterPipe\bin"
}
$InstallDir = [System.IO.Path]::GetFullPath($InstallDir)
$root = [System.IO.Path]::GetPathRoot($InstallDir)
if ($InstallDir -eq $root) {
    throw "refusing unsafe install directory '$InstallDir'"
}
if ($Purge -and -not $Uninstall) {
    throw "-Purge requires -Uninstall"
}

function Remove-UserPathEntry([string]$Path) {
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ([string]::IsNullOrWhiteSpace($userPath)) { return }
    $kept = @($userPath.Split(';') | Where-Object {
        -not [string]::IsNullOrWhiteSpace($_) -and
        -not [string]::Equals($_.TrimEnd('\'), $Path.TrimEnd('\'), [StringComparison]::OrdinalIgnoreCase)
    })
    [Environment]::SetEnvironmentVariable("Path", ($kept -join ';'), "User")
}

if ($Uninstall) {
    foreach ($program in $Programs) {
        $path = Join-Path $InstallDir $program
        Remove-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue
        Write-Host "removed $path"
    }
    if ((Test-Path -LiteralPath $InstallDir) -and
        -not (Get-ChildItem -LiteralPath $InstallDir -Force | Select-Object -First 1)) {
        Remove-Item -LiteralPath $InstallDir -Force
        Remove-UserPathEntry $InstallDir
    }
    if ($Purge -and -not [string]::IsNullOrWhiteSpace($ProviderSlug)) {
        $assets = Join-Path $env:LOCALAPPDATA "UtterPipe\providers\$ProviderSlug"
        Remove-Item -LiteralPath $assets -Recurse -Force -ErrorAction SilentlyContinue
        Write-Host "removed provider assets for $ProviderSlug (not recoverable)"
    }
    exit 0
}

if ($env:PROCESSOR_ARCHITECTURE -notin @("AMD64", "x86_64")) {
    throw "no Windows release artifact is published for $env:PROCESSOR_ARCHITECTURE"
}
$Target = "x86_64-pc-windows-msvc"

if ([string]::IsNullOrWhiteSpace($Version)) {
    $headers = @{ Accept = "application/vnd.github+json" }
    if (-not [string]::IsNullOrWhiteSpace($env:GITHUB_TOKEN)) {
        $headers.Authorization = "Bearer $env:GITHUB_TOKEN"
    }
    $release = Invoke-RestMethod -Headers $headers -Uri "https://api.github.com/repos/$Repository/releases/latest"
    $Version = $release.tag_name
}
if ($Version -notmatch '^v[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z.-]+)?$') {
    throw "invalid release version '$Version'"
}

$archive = "$ArchivePrefix-$Version-$Target.zip"
if ([string]::IsNullOrWhiteSpace($ReleaseBaseUrl)) {
    $ReleaseBaseUrl = "https://github.com/$Repository/releases/download/$Version"
}
$temporary = Join-Path ([System.IO.Path]::GetTempPath()) ("utterpipe-install-" + [guid]::NewGuid())
New-Item -ItemType Directory -Path $temporary | Out-Null
try {
    $archivePath = Join-Path $temporary $archive
    $checksumPath = "$archivePath.sha256"
    Invoke-WebRequest -UseBasicParsing -Uri "$ReleaseBaseUrl/$archive" -OutFile $archivePath
    Invoke-WebRequest -UseBasicParsing -Uri "$ReleaseBaseUrl/$archive.sha256" -OutFile $checksumPath
    $expected = ((Get-Content -LiteralPath $checksumPath -Raw).Trim() -split '\s+')[0].ToLowerInvariant()
    $actual = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -cne $expected) {
        throw "release archive checksum mismatch"
    }

    Expand-Archive -LiteralPath $archivePath -DestinationPath $temporary
    $packageRoot = Join-Path $temporary "$ArchivePrefix-$Version-$Target"
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    foreach ($program in $Programs) {
        $source = Join-Path $packageRoot $program
        $destination = Join-Path $InstallDir $program
        Copy-Item -LiteralPath $source -Destination $destination -Force
        Write-Host "installed $destination"
    }

    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $entries = @($userPath -split ';')
    if (-not ($entries | Where-Object {
        [string]::Equals($_.TrimEnd('\'), $InstallDir.TrimEnd('\'), [StringComparison]::OrdinalIgnoreCase)
    })) {
        $newPath = if ([string]::IsNullOrWhiteSpace($userPath)) { $InstallDir } else { "$userPath;$InstallDir" }
        [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
        Write-Host "added $InstallDir to the user PATH; open a new terminal to use it"
    }
} finally {
    Remove-Item -LiteralPath $temporary -Recurse -Force -ErrorAction SilentlyContinue
}
