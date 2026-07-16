# Speak-friendly status briefing for Jarvis: system (RAM/battery/apps) + the
# most-recently-active project under brain/raw. Prints one line of prose that
# Jarvis reads aloud.
$ErrorActionPreference = 'SilentlyContinue'

$parts = @()

# --- System ---
$os = Get-CimInstance Win32_OperatingSystem
$totalKB = $os.TotalVisibleMemorySize
$freeKB = $os.FreePhysicalMemory
$usedPct = [int]((($totalKB - $freeKB) / $totalKB) * 100)
$freeGB = [math]::Round($freeKB / 1MB, 1)
$bat = (Get-CimInstance Win32_Battery).EstimatedChargeRemaining

$sys = "Here's your update. Memory is at $usedPct percent, with $freeGB gigabytes free"
if ($bat) { $sys += ", and the battery is at $bat percent" }
$sys += "."
$parts += $sys

$apps = @(Get-Process | Where-Object { $_.MainWindowTitle } | Select-Object -ExpandProperty ProcessName -Unique)
if ($apps.Count -gt 0) {
    $names = ($apps | Select-Object -First 3) -join ', '
    $parts += "You have $($apps.Count) apps open, including $names."
}

# --- Most recent project ---
# Projects folder to report on: JARVIS_PROJECTS env var, else ~\brain\raw.
$raw = if ($env:JARVIS_PROJECTS) { $env:JARVIS_PROJECTS } else { Join-Path $env:USERPROFILE 'brain\raw' }
$proj = Get-ChildItem $raw -Directory | Sort-Object LastWriteTime -Descending | Select-Object -First 1
if ($proj) {
    $ago = New-TimeSpan -Start $proj.LastWriteTime -End (Get-Date)
    $when = if ($ago.TotalMinutes -lt 60) { "$([int]$ago.TotalMinutes) minutes ago" }
            elseif ($ago.TotalHours -lt 24) { "$([int]$ago.TotalHours) hours ago" }
            else { "$([int]$ago.TotalDays) days ago" }
    $parts += "Your most recently active project is $($proj.Name), last touched $when."
}

Write-Output ($parts -join ' ')
