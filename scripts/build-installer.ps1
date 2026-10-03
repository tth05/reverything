# Builds the binaries and the installer (target\installer\reverything-setup-<version>.exe).
# Needs Inno Setup 6 and the Windows SDK (for fxc.exe, the shader compiler GPUI uses).
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

$iscc = @(
    "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe",
    "$env:ProgramFiles\Inno Setup 6\ISCC.exe",
    "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
) | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $iscc) { throw 'Inno Setup 6 not found (https://jrsoftware.org/isinfo.php, or: winget install JRSoftware.InnoSetup)' }

$binDir = Join-Path $repo "target\$Profile"
& $iscc "/DAppVersion=$version" "/DBinDir=$binDir" (Join-Path $repo 'installer\reverything.iss')
if ($LASTEXITCODE -ne 0) { throw 'ISCC failed' }
Get-Item (Join-Path $repo "target\installer\reverything-setup-$version.exe")
