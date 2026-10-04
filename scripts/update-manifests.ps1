# Writes the package manager manifests for a released version:
#   bucket\reverything.json                  Scoop (this repository is a bucket)
#   packaging\winget\manifests\t\tth05\Reverything\<version>\*.yaml
#                                            winget, in the layout of microsoft/winget-pkgs
#
# The SHA256 comes from -Installer (a local reverything-setup-<version>.exe, as on CI) or from the
# .sha256 file published with the GitHub release.
#
#   scripts\update-manifests.ps1 -Version 0.2.0
#   scripts\update-manifests.ps1 -Version 0.2.0 -Installer target\installer\reverything-setup-0.2.0.exe
param(
    [Parameter(Mandatory)] [string] $Version,
    [string] $Installer = ""
)
$ErrorActionPreference = 'Stop'

$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$name = "reverything-setup-$Version.exe"
$url = "https://github.com/tth05/reverything/releases/download/v$Version/$name"

if ($Installer) {
    $sha256 = (Get-FileHash $Installer -Algorithm SHA256).Hash.ToLower()
} else {
    $sha256 = ((Invoke-RestMethod "$url.sha256") -split '\s+')[0].ToLower()
}
if ($sha256 -notmatch '^[0-9a-f]{64}$') { throw "No valid SHA256 for $name" }

# Scoop runs the real installer: the service has to be registered, which extracting it would skip.
# /MANAGED tells the app to leave updates to the package manager (both manifests pass it).
$scoop = [ordered]@{
    version     = $Version
    description = 'Blazingly fast, resource-efficient global file search for Windows'
    homepage    = 'https://github.com/tth05/reverything'
    license     = [ordered]@{
        identifier = 'Proprietary'
        url        = 'https://github.com/tth05/reverything/blob/master/LICENSE'
    }
    notes       = 'Installs the Reverything Index service, which asks for administrator rights once.'
    url         = "$url#/setup.exe"
    hash        = $sha256
    installer   = [ordered]@{
        script = @(
            'Start-Process "$dir\setup.exe" -ArgumentList ''/VERYSILENT'', ''/SUPPRESSMSGBOXES'', ''/NORESTART'', ''/MANAGED=scoop'' -Wait'
        )
    }
    uninstaller = [ordered]@{
        script = @(
            '$uninstaller = "$env:ProgramFiles\Reverything\unins000.exe"',
            'if (Test-Path $uninstaller) { Start-Process $uninstaller -ArgumentList ''/VERYSILENT'', ''/SUPPRESSMSGBOXES'' -Wait }'
        )
    }
    checkver    = 'github'
    autoupdate  = [ordered]@{
        url  = 'https://github.com/tth05/reverything/releases/download/v$version/reverything-setup-$version.exe#/setup.exe'
        hash = [ordered]@{ url = 'https://github.com/tth05/reverything/releases/download/v$version/reverything-setup-$version.exe.sha256' }
    }
}
New-Item -ItemType Directory -Force (Join-Path $repo 'bucket') | Out-Null
$scoop | ConvertTo-Json -Depth 5 | Set-Content (Join-Path $repo 'bucket\reverything.json') -Encoding utf8NoBOM

# winget, in the form microsoft/winget-pkgs expects: schema 1.12.0, CRLF line endings
$id = 'tth05.Reverything'
$dir = Join-Path $repo "packaging\winget\manifests\t\tth05\Reverything\$Version"
New-Item -ItemType Directory -Force $dir | Out-Null
$schema = '1.12.0'
$date = Get-Date -Format 'yyyy-MM-dd'
function Write-Manifest([string] $File, [string] $Text) {
    $crlf = ($Text.Trim() -replace "`r?`n", "`r`n") + "`r`n"
    [IO.File]::WriteAllText((Join-Path $dir $File), $crlf, [Text.UTF8Encoding]::new($false))
}
Write-Manifest "$id.yaml" @"
# yaml-language-server: `$schema=https://aka.ms/winget-manifest.version.$schema.schema.json

PackageIdentifier: $id
PackageVersion: $Version
DefaultLocale: en-US
ManifestType: version
ManifestVersion: $schema
"@
Write-Manifest "$id.installer.yaml" @"
# yaml-language-server: `$schema=https://aka.ms/winget-manifest.installer.$schema.schema.json

PackageIdentifier: $id
PackageVersion: $Version
InstallerType: inno
Scope: machine
InstallModes:
- interactive
- silent
- silentWithProgress
InstallerSwitches:
  Custom: /MANAGED=winget
UpgradeBehavior: install
ElevationRequirement: elevatesSelf
ProductCode: '{6B0E2F47-9C1D-4E4B-A6E8-3F2C8D9B5A71}_is1'
ReleaseDate: $date
Installers:
- Architecture: x64
  InstallerUrl: $url
  InstallerSha256: $($sha256.ToUpper())
ManifestType: installer
ManifestVersion: $schema
"@
Write-Manifest "$id.locale.en-US.yaml" @"
# yaml-language-server: `$schema=https://aka.ms/winget-manifest.defaultLocale.$schema.schema.json

PackageIdentifier: $id
PackageVersion: $Version
PackageLocale: en-US
Publisher: tth05
PublisherUrl: https://github.com/tth05
PublisherSupportUrl: https://github.com/tth05/reverything/issues
Author: tth05
PackageName: Reverything
PackageUrl: https://github.com/tth05/reverything
License: Proprietary (indexing library MIT)
LicenseUrl: https://github.com/tth05/reverything/blob/master/LICENSE
Copyright: Copyright (c) 2026 tth05
ShortDescription: Blazingly fast, resource-efficient global file search for Windows. Can index millions of files in seconds.
Description: |-
  Reverything finds any file on your PC by name, as you type. It reads the file tables of NTFS drives directly, so millions of files are indexed in seconds, and keeps the index up to date through the NTFS change journal.
  Searches support wildcards, folders, exclusions and size or date filters. Results can be opened, dragged into other programs, copied or shown in Explorer.
  A small background service, running as the system account, keeps the index and answers the searches; the search window runs as the user, lives in the tray and comes up with a global shortcut. While the window is not in use, the service uses almost no CPU or memory.
Moniker: reverything
Tags:
- everything
- everything-alternative
- file-search
- file-finder
- files
- mft
- ntfs
- search
- usn-journal
ReleaseNotesUrl: https://github.com/tth05/reverything/releases/tag/v$Version
ManifestType: defaultLocale
ManifestVersion: $schema
"@

Write-Host "Wrote bucket\reverything.json and $dir for $Version ($sha256)"
