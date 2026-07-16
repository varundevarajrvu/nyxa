# Play a specific song on Spotify with NO API key. Opens the desktop app's
# search for the query, brings the window to the foreground (some Spotify
# controls only honor Invoke when their window is active), then uses UI
# Automation to find and invoke the Play button. Two button shapes are handled:
#   - Search results  -> a button named "Play <song> by <artist>" (match query)
#   - Exact-match jump -> the track page's primary button, named just "Play"
# Invoke is tried first; if the control doesn't support it we fall back to the
# legacy accessibility DoDefaultAction. Retries for several seconds while the
# results render. Query comes from the reviewed transcript param.
param(
    [Parameter(Mandatory = $true)]
    [string]$Query
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes

# Win32 to force the Spotify window foreground (Invoke on Chromium-hosted
# controls is unreliable while the window is in the background).
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class Fg {
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int n);
    [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
}
"@

function Get-SpotifyWindow {
    Get-Process Spotify -ErrorAction SilentlyContinue |
        Where-Object { $_.MainWindowHandle -ne 0 } | Select-Object -First 1
}

Start-Process ("spotify:search:" + [uri]::EscapeDataString($Query))
Start-Sleep -Seconds 5

# Foreground the window so control invokes are honored.
$sp = Get-SpotifyWindow
if ($sp) {
    [Fg]::ShowWindow($sp.MainWindowHandle, 9) | Out-Null   # SW_RESTORE
    [Fg]::BringWindowToTop($sp.MainWindowHandle) | Out-Null
    [Fg]::SetForegroundWindow($sp.MainWindowHandle) | Out-Null
    Start-Sleep -Milliseconds 400
}

$words = @($Query.ToLower() -split '\s+' | Where-Object { $_.Length -gt 1 })

function Find-PlayButton {
    $sp = Get-SpotifyWindow
    if (-not $sp) { return $null }
    $root = [System.Windows.Automation.AutomationElement]::FromHandle($sp.MainWindowHandle)
    $cond = New-Object System.Windows.Automation.PropertyCondition(
        [System.Windows.Automation.AutomationElement]::ControlTypeProperty,
        [System.Windows.Automation.ControlType]::Button)
    $btns = $root.FindAll([System.Windows.Automation.TreeScope]::Descendants, $cond)

    # 1) A "Play <song>" button whose name contains all query words.
    foreach ($b in $btns) {
        $n = $b.Current.Name; if (-not $n) { continue }
        $nl = $n.ToLower()
        if (-not $nl.StartsWith('play ')) { continue }
        $all = $true
        foreach ($w in $words) { if ($nl -notmatch [regex]::Escape($w)) { $all = $false; break } }
        if ($all) { return $b }
    }
    # 2) The track page's primary button, named exactly "Play".
    foreach ($b in $btns) {
        if ($b.Current.Name -eq 'Play') { return $b }
    }
    return $null
}

# Try to activate an element: InvokePattern first, then legacy DoDefaultAction.
function Invoke-Element($el) {
    try {
        $p = $el.GetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern)
        $p.Invoke()
        return $true
    } catch {}
    try {
        $legacy = [System.Windows.Automation.AutomationElement]::LegacyIAccessiblePatternIdentifiers
        $lp = $el.GetCurrentPattern($legacy.Pattern)
        $lp.DoDefaultAction()
        return $true
    } catch {}
    return $false
}

$target = $null
for ($try = 0; $try -lt 10 -and -not $target; $try++) {
    $target = Find-PlayButton
    if (-not $target) { Start-Sleep -Milliseconds 900 }
}

if (-not $target) { Write-Output "no-play-button"; exit 2 }

# Re-foreground right before invoking (search navigation can steal focus).
$sp = Get-SpotifyWindow
if ($sp) { [Fg]::SetForegroundWindow($sp.MainWindowHandle) | Out-Null; Start-Sleep -Milliseconds 200 }

if (Invoke-Element $target) {
    Write-Output ("invoked: " + $target.Current.Name)
    exit 0
} else {
    Write-Output ("invoke-failed: " + $target.Current.Name)
    exit 3
}
