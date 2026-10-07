# Portable installer/updater for yak on Windows: downloads the prebuilt
# release binary from GitHub into ~\.local\bin (user-level, no admin needed),
# verifies its sha256 and updates the user PATH. Re-running it updates in
# place: it checks the latest GitHub release, prints the version move when
# the installed binary is behind, and leaves an equal version alone.
#
#   irm https://jiaoyuan.org/yak/install.ps1 | iex
#
# Environment overrides:
#   $env:YAK_VERSION = "v0.1.0"          pin a release tag (default: latest)
#   $env:YAK_REPO = "owner/repo"         install from a fork
#   $env:YAK_INSTALL_DIR = "C:\bin"      install directory (default: ~\.local\bin)
#   $env:YAK_FORCE = "1"                 reinstall even when the version is unchanged
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$Repo = if ($env:YAK_REPO) { $env:YAK_REPO } else { "imjiaoyuan/yak" }
$InstallDir = if ($env:YAK_INSTALL_DIR) { $env:YAK_INSTALL_DIR } else { "$env:USERPROFILE\.local\bin" }
$Force = $env:YAK_FORCE -eq "1"

if ($env:PROCESSOR_ARCHITECTURE -ne "AMD64") {
    Write-Error "unsupported architecture $($env:PROCESSOR_ARCHITECTURE) (only x86_64 builds exist today)"
}
$Target = "x86_64-pc-windows-msvc"

if ($env:YAK_VERSION) {
    $Version = $env:YAK_VERSION
} else {
    $latest = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest"
    $Version = $latest.tag_name
    if (-not $Version) { Write-Error "could not resolve the latest release (set `$env:YAK_VERSION to pin)" }
}

$Archive = "yak-$Target.zip"
$Checksum = "yak-$Target.sha256"
$Base = "https://github.com/$Repo/releases/download/$Version"

Write-Host "==> $Repo $Version ($Target)"
$tmp = New-Item -ItemType Directory -Force -Path (Join-Path $env:TEMP ([System.IO.Path]::GetRandomFileName()))
try {
    Write-Host "==> downloading $Base/$Archive"
    Invoke-WebRequest "$Base/$Archive" -OutFile "$tmp\$Archive"
    Invoke-WebRequest "$Base/$Checksum" -OutFile "$tmp\$Checksum"

    Write-Host "==> verifying sha256"
    $expected = (Get-Content "$tmp\$Checksum" -Raw).Trim() -split '\s+' | Select-Object -First 1
    $actual = (Get-FileHash "$tmp\$Archive" -Algorithm SHA256).Hash.ToLower()
    if ($expected -ne $actual) { Write-Error "checksum mismatch — try again" }

    Expand-Archive "$tmp\$Archive" -DestinationPath $tmp -Force
    # `--version` prints `yak, version X.Y.Z`; keep just the version so the
    # `updating old -> new` line is clean (a no-match returns the raw output).
    $NewVer = (& "$tmp\yak.exe" --version) -replace '^yak, version (.*)$', '$1'

    # update semantics: same version stays put unless YAK_FORCE=1
    if (Test-Path "$InstallDir\yak.exe") {
        $OldVer = $null
        try { $OldVer = (& "$InstallDir\yak.exe" --version) -replace '^yak, version (.*)$', '$1' } catch {}
        if ($OldVer -and $OldVer -eq $NewVer -and -not $Force) {
            Write-Host "==> already $NewVer at $InstallDir\yak.exe (up to date; YAK_FORCE=1 reinstalls)"
            return
        }
        if ($OldVer -and $OldVer -ne $NewVer) { Write-Host "==> updating $OldVer -> $NewVer" }
    }

    Write-Host "==> installing to $InstallDir"
    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Move-Item -Force "$tmp\yak.exe" "$InstallDir\yak.exe"

    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($userPath -notlike "*$InstallDir*") {
        [Environment]::SetEnvironmentVariable("Path", "$userPath;$InstallDir", "User")
        Write-Host "==> added $InstallDir to the user PATH (restart your terminal to pick it up)"
    }
} finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}

& "$InstallDir\yak.exe" --version
