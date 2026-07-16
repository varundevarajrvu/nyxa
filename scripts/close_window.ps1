# Close the browser TAB showing a website. Finds the visible top-level window
# whose title contains $TitleMatch (a browser window shows its ACTIVE tab's
# title), reliably foregrounds it, and sends Ctrl+W — closing just that tab,
# not the whole browser. TitleMatch comes from the reviewed [web_title] map,
# passed as an argument. Jarvis web.close executor.
#
# Foregrounding from a background process needs the standard focus-unlock dance
# (ALT tap + AttachThreadInput); a plain SetForegroundWindow is ignored by
# Windows and the keystroke would go nowhere.
param(
    [Parameter(Mandatory = $true)]
    [string]$TitleMatch
)

$ErrorActionPreference = 'Stop'

Add-Type @"
using System;
using System.Text;
using System.Runtime.InteropServices;
public class JWin {
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc cb, IntPtr l);
    [DllImport("user32.dll")] public static extern int GetWindowText(IntPtr h, StringBuilder s, int n);
    [DllImport("user32.dll")] public static extern int GetWindowTextLength(IntPtr h);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, IntPtr pid);
    [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
    [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint a, uint b, bool attach);
    [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);

    public static IntPtr Find(string match) {
        IntPtr found = IntPtr.Zero;
        string m = match.ToLower();
        EnumWindows((h, l) => {
            if (!IsWindowVisible(h)) return true;
            int len = GetWindowTextLength(h);
            if (len == 0) return true;
            var sb = new StringBuilder(len + 1);
            GetWindowText(h, sb, sb.Capacity);
            if (sb.ToString().ToLower().Contains(m)) { found = h; return false; }
            return true;
        }, IntPtr.Zero);
        return found;
    }

    public static void Focus(IntPtr hwnd) {
        uint fg = GetWindowThreadProcessId(GetForegroundWindow(), IntPtr.Zero);
        uint me = GetCurrentThreadId();
        keybd_event(0x12, 0, 0, UIntPtr.Zero);          // ALT down -> unlock foreground
        AttachThreadInput(me, fg, true);
        ShowWindow(hwnd, 9);                            // SW_RESTORE
        BringWindowToTop(hwnd);
        SetForegroundWindow(hwnd);
        AttachThreadInput(me, fg, false);
        keybd_event(0x12, 0, 0x0002, UIntPtr.Zero);     // ALT up (KEYEVENTF_KEYUP)
    }
}
"@

$h = [JWin]::Find($TitleMatch)
if ($h -eq [IntPtr]::Zero) {
    Write-Output "not-found"
    exit 2
}

[JWin]::Focus($h)
Start-Sleep -Milliseconds 350

Add-Type -AssemblyName System.Windows.Forms
[System.Windows.Forms.SendKeys]::SendWait("^w")

Write-Output "closed-tab"
exit 0
