#Requires -RunAsAdministrator
# Removes the "ReverythingBench" scheduled task created by register-bench-task.ps1.

$ErrorActionPreference = 'Stop'

$taskName = 'ReverythingBench'

if (Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue) {
    Unregister-ScheduledTask -TaskName $taskName -Confirm:$false
    Write-Host "Removed scheduled task '$taskName'"
} else {
    Write-Host "Scheduled task '$taskName' does not exist"
}
