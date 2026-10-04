# Builds the binaries and the installer (target\installer\reverything-setup-<version>.exe).
# Needs Inno Setup 6, the Windows SDK (for fxc.exe, the shader compiler GPUI uses) and
# cargo-about for the third-party licenses (cargo install cargo-about --locked --features cli).
#
#   scripts\build-installer.ps1                  release profile, quick to build
#   scripts\build-installer.ps1 -Profile dist    fat LTO, what CI ships

param([ValidateSet('release', 'dist')] [string] $Profile = 'release')

$ErrorActionPreference = 'Stop'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path

# GPUI's build script picks the newest SDK, which does not always contain fxc.exe
if (-not $env:GPUI_FXC_PATH) {
    $fxc = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\fxc.exe" -ErrorAction SilentlyContinue |
        Sort-Object FullName -Descending | Select-Object -First 1
    if (-not $fxc) { throw 'fxc.exe not found, install the Windows SDK' }
    $env:GPUI_FXC_PATH = $fxc.FullName
}

$version = (Select-String -Path (Join-Path $repo 'Cargo.toml') -Pattern '^version = "(.+)"' |
    Select-Object -First 1).Matches[0].Groups[1].Value

Push-Location $repo
try {
    cargo build --workspace --profile $Profile
    if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }
} finally {
    Pop-Location
}

# The licenses of the libraries in the binaries, shipped next to them and shown in About
$binDir = Join-Path $repo "target\$Profile"
$notices = Join-Path $binDir 'THIRD-PARTY-NOTICES.html'
Push-Location $repo
try {
    cargo about generate about.hbs -o $notices
    if ($LASTEXITCODE -ne 0) { throw 'cargo about failed (cargo install cargo-about --locked --features cli)' }
} finally {
    Pop-Location
}

$iscc = @(
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe",
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw 'Inno Setup 6 not found (https://jrsoftware.org/isinfo.php, or: winget install JRSoftware.InnoSetup)' }

& $iscc "/DAppVersion=$version" "/DBinDir=$binDir" (Join-Path $repo 'installer\reverything.iss')
if ($LASTEXITCODE -ne 0) { throw 'ISCC failed' }
Get-Item (Join-Path $repo "target\installer\reverything-setup-$version.exe")
