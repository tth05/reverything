#Requires -RunAsAdministrator
# Registers the "ReverythingBench" scheduled task, which runs `reverything.exe --bench` elevated
# without a UAC prompt. Start it with `schtasks /run /tn ReverythingBench`; the output goes to
# target\bench.log. The task can only run this fixed command, for at most 2 minutes.
#
# Run once from an elevated PowerShell. Remove it again with unregister-bench-task.ps1.

$ErrorActionPreference = 'Stop'

$taskName = 'ReverythingBench'
$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$exe = Join-Path $repo 'target\release\reverything.exe'
$log = Join-Path $repo 'target\bench.log'

$action = New-ScheduledTaskAction `
    -Execute 'cmd.exe' `
    -Argument "/c `"`"$exe`" --bench > `"$log`" 2>&1`"" `
    -WorkingDirectory $repo
$principal = New-ScheduledTaskPrincipal `
    -UserId "$env:USERDOMAIN\$env:USERNAME" `
    -LogonType Interactive `
    -RunLevel Highest
$settings = New-ScheduledTaskSettingsSet `
    -ExecutionTimeLimit (New-TimeSpan -Minutes 2) `
    -MultipleInstances IgnoreNew

Register-ScheduledTask `
    -TaskName $taskName `
    -Action $action `
    -Principal $principal `
    -Settings $settings `
    -Force | Out-Null

Write-Host "Registered scheduled task '$taskName' running $exe --bench"
