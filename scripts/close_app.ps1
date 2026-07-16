# Gracefully close an application by process name (WM_CLOSE via
# CloseMainWindow) — NEVER a force-kill. Jarvis app.close executor.
# ProcessName comes from the reviewed [close_process] registry map, passed as
# an argument (never spliced into command text).
param(
    [Parameter(Mandatory = $true)]
    [string]$ProcessName
)

$ErrorActionPreference = 'Stop'

$procs = @(Get-Process -Name $ProcessName -ErrorAction SilentlyContinue |
    Where-Object { $_.MainWindowHandle -ne 0 })

if ($procs.Count -eq 0) {
    Write-Output "not-running"
    exit 2
}

$closed = 0
foreach ($p in $procs) {
    # CloseMainWindow posts WM_CLOSE — the app can prompt to save, exactly like
    # the user clicking the X. Returns $false if the window is unresponsive.
    if ($p.CloseMainWindow()) { $closed++ }
}

Write-Output "closed=$closed"
if ($closed -eq 0) { exit 3 }
exit 0
