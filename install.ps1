[CmdletBinding()]
param(
    [string]$Version = $env:VERSION,
    [string]$InstallDir = $env:INSTALL_DIR,
    [string]$ReleaseBaseUrl = $env:RELEASE_BASE_URL,
    [switch]$Uninstall,
    [switch]$Purge,
    [switch]$NoSetup
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

$installedExecutable = Join-Path $InstallDir "agent-speak.exe"
$installedVersion = (& $installedExecutable --version 2>$null | Select-Object -First 1)
Write-Host ""
Write-Host "Agent Speak installation complete."
Write-Host "  Executable: $installedExecutable"
if (-not [string]::IsNullOrWhiteSpace($installedVersion)) {
    Write-Host "  Version: $installedVersion"
}
Write-Host "  Checksum: verified"

function Test-InteractiveConsole {
    try {
        return -not [Console]::IsInputRedirected -and -not [Console]::IsOutputRedirected
    } catch {
        return $false
    }
}

function Confirm-DefaultYes([string]$Question) {
    $answer = Read-Host "$Question [Y/n]"
    return [string]::IsNullOrWhiteSpace($answer) -or $answer -match '^(?i:y|yes)$'
}

$profilePath = Join-Path $HOME ".agent-speak.toml"
$profileReady = Test-Path -LiteralPath $profilePath -PathType Leaf
if ($profileReady) {
    Write-Host "  Profile: using existing $profilePath"
} elseif (-not $NoSetup -and (Test-InteractiveConsole) -and
          (Confirm-DefaultYes "Create the optional user profile at $profilePath?")) {
    & $installedExecutable config create --output $profilePath
    if ($LASTEXITCODE -eq 0) {
        $profileReady = $true
    } else {
        Write-Warning "Profile creation failed; Agent Speak can still use its built-in quick profile."
    }
}

if (-not $NoSetup -and (Test-InteractiveConsole)) {
    if (Get-Command codex -ErrorAction SilentlyContinue) {
        & codex mcp get agent-speak *> $null
        if ($LASTEXITCODE -eq 0) {
            Write-Host "  Codex MCP: existing agent-speak entry preserved"
        } elseif (Confirm-DefaultYes "Codex detected. Register Agent Speak for this user now?") {
            if ($profileReady) {
                & codex mcp add agent-speak -- $installedExecutable serve --config $profilePath
            } else {
                & codex mcp add agent-speak -- $installedExecutable serve --quick
            }
            if ($LASTEXITCODE -ne 0) {
                Write-Warning "Codex registration failed; the Agent Speak installation is still usable."
            }
        }
    }
    if (Get-Command claude -ErrorAction SilentlyContinue) {
        & claude mcp get agent-speak *> $null
        if ($LASTEXITCODE -eq 0) {
            Write-Host "  Claude MCP: existing agent-speak entry preserved"
        } elseif (Confirm-DefaultYes "Claude Code detected. Register Agent Speak for this user now?") {
            if ($profileReady) {
                & claude mcp add --scope user agent-speak -- $installedExecutable serve --config $profilePath
            } else {
                & claude mcp add --scope user agent-speak -- $installedExecutable serve --quick
            }
            if ($LASTEXITCODE -ne 0) {
                Write-Warning "Claude Code registration failed; the Agent Speak installation is still usable."
            }
        }
    }
    if ((Get-Command opencode -ErrorAction SilentlyContinue) -or
        (Get-Command opencode2 -ErrorAction SilentlyContinue)) {
        Write-Host "  OpenCode detected: its MCP setup varies by version; see https://opencode.ai/docs/mcp-servers/"
    }
    if (Get-Command code -ErrorAction SilentlyContinue) {
        Write-Host "  VS Code detected: for automatic local playback in Remote SSH, install extension 4piu.agent-speak."
    }
}

Write-Host ""
Write-Host "Next steps:"
if ($profileReady) {
    Write-Host "  1. Validate: `"$installedExecutable`" validate --config `"$profilePath`""
} else {
    Write-Host "  1. Validate the built-in quick profile: `"$installedExecutable`" validate"
    Write-Host "     Create a profile later: `"$installedExecutable`" config create"
}
Write-Host "  2. Register Agent Speak with your MCP host, then restart the host."
Write-Host '  3. Ask your agent: Say "Agent Speak is ready" out loud.'
