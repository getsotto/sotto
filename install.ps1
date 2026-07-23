#Requires -Version 5.1
# Sotto installer (Windows): download the latest release, verify its checksum (and its Sigstore
# signature when `cosign` is installed), and install the `sotto` binary.
#
#   irm https://raw.githubusercontent.com/getsotto/sotto/main/install.ps1 | iex
#
# Options (environment variables - same names as install.sh):
#   SOTTO_INSTALL_DIR  install directory        (default: $env:LOCALAPPDATA\sotto\bin)
#   SOTTO_VERSION      tag to install, e.g. v0.1.0  (default: latest release)
#
# Does nothing that needs elevation and touches only the install directory.
#
# The whole body runs inside `& { ... }`, not as bare top-level statements: `irm | iex` evaluates
# this script in the CALLER's own scope, not a subprocess (unlike `install.sh` under `curl | sh`,
# which forks a real subshell). Without that boundary, `exit` on a failure path would kill the
# user's whole PowerShell session, and `$ErrorActionPreference` below would leak into it too. The
# call operator gives this script its own scope, so `throw` (a script-terminating error, not a
# process exit) stays contained to this invocation.
& {
    $ErrorActionPreference = "Stop"
    $ProgressPreference = "SilentlyContinue" # Invoke-WebRequest's progress bar is otherwise very slow.

    function Fail($Message) {
        throw "error: $Message"
    }

    $Repo = "getsotto/sotto"
    $InstallDir = if ($env:SOTTO_INSTALL_DIR) { $env:SOTTO_INSTALL_DIR } else { "$env:LOCALAPPDATA\sotto\bin" }

    # --- pick the release target for this machine ------------------------------------------------
    $Arch = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture
    if ($Arch -ne "X64") {
        Fail "no prebuilt binary for Windows/$Arch - build from source: cargo build --release -p sotto-cli"
    }
    $Target = "x86_64-pc-windows-msvc"

    # --- resolve the version -----------------------------------------------------------------------
    $Version = $env:SOTTO_VERSION
    if (-not $Version) {
        $Release = Invoke-RestMethod -UseBasicParsing -Uri "https://api.github.com/repos/$Repo/releases/latest"
        $Version = $Release.tag_name
        if (-not $Version) {
            Fail "could not determine the latest release (set `$env:SOTTO_VERSION = 'vX.Y.Z')"
        }
    }

    $Asset = "sotto-$Version-$Target.zip"
    $Base = "https://github.com/$Repo/releases/download/$Version"
    Write-Host "installing sotto $Version ($Target)"

    # --- download + verify ---------------------------------------------------------------------------
    $Tmp = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())
    New-Item -ItemType Directory -Path $Tmp | Out-Null
    try {
        $AssetPath = Join-Path $Tmp $Asset
        $SumsPath = Join-Path $Tmp "SHA256SUMS"
        try {
            Invoke-WebRequest -UseBasicParsing -Uri "$Base/$Asset" -OutFile $AssetPath
            Invoke-WebRequest -UseBasicParsing -Uri "$Base/SHA256SUMS" -OutFile $SumsPath
        } catch {
            Fail "download failed: $Base/$Asset ($_)"
        }

        $ExpectedLine = Select-String -Path $SumsPath -Pattern "  $Asset$"
        if (-not $ExpectedLine) {
            Fail "$Asset is not listed in SHA256SUMS"
        }
        $Expected = ($ExpectedLine.Line -split "\s+")[0]
        $Actual = (Get-FileHash -Algorithm SHA256 -Path $AssetPath).Hash
        if ($Expected -ne $Actual) {
            Fail "checksum verification FAILED - refusing to install"
        }
        Write-Host "checksum verified"

        # Signature check: keyless Sigstore signatures bind the artefact to the release workflow's
        # identity. Opportunistic - run when cosign is available; SECURITY.md has the manual steps.
        # Only the bundle *download* is soft-fail (no bundle published yet is a normal state); a
        # download that succeeds but then fails verification must NOT be caught by the same
        # handler, or a genuinely bad signature would silently downgrade to "checksum-only".
        $Cosign = Get-Command cosign -ErrorAction SilentlyContinue
        if ($Cosign) {
            $BundlePath = Join-Path $Tmp "$Asset.sigstore.json"
            $BundleDownloaded = $true
            try {
                Invoke-WebRequest -UseBasicParsing -Uri "$Base/$Asset.sigstore.json" -OutFile $BundlePath
            } catch {
                $BundleDownloaded = $false
            }
            if ($BundleDownloaded) {
                & cosign verify-blob `
                    --bundle $BundlePath `
                    --certificate-identity-regexp "^https://github.com/$Repo/.github/workflows/release.yml@refs/tags/v" `
                    --certificate-oidc-issuer https://token.actions.githubusercontent.com `
                    $AssetPath *> $null
                if ($LASTEXITCODE -ne 0) {
                    Fail "Sigstore verification FAILED - refusing to install"
                }
                Write-Host "signature verified (Sigstore)"
            } else {
                Write-Host "note: no signature bundle found for this release; checksum-only install"
            }
        } else {
            Write-Host "note: cosign not installed; skipping signature verification (see SECURITY.md)"
        }

        # --- install -----------------------------------------------------------------------------------
        Expand-Archive -Path $AssetPath -DestinationPath $Tmp -Force
        New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
        Copy-Item -Path (Join-Path $Tmp "sotto-$Version-$Target\sotto.exe") -Destination (Join-Path $InstallDir "sotto.exe") -Force

        Write-Host "installed $InstallDir\sotto.exe"
        $PathDirs = $env:PATH -split ";"
        if ($PathDirs -notcontains $InstallDir) {
            Write-Host "note: $InstallDir is not on your PATH - add it, e.g.:"
            Write-Host "  [Environment]::SetEnvironmentVariable('PATH', `"`$env:PATH;$InstallDir`", 'User')"
        }
        Write-Host "shell completions: sotto completions powershell (also bundled in the release zip)"
        Write-Host "get started: sotto init"
    } finally {
        Remove-Item -Recurse -Force $Tmp -ErrorAction SilentlyContinue
    }
}
