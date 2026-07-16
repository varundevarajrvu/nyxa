# Move a file to the Recycle Bin (recoverable — never a permanent delete).
# Jarvis file.delete executor. Path is a fully-resolved absolute path from the
# reviewed resolver (canonicalized + profile-jailed in Rust), passed as a
# discrete argument. The script re-checks it is a real file before acting.
param(
    [Parameter(Mandatory = $true)]
    [string]$Path
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
    Write-Output "not-a-file"
    exit 2
}

# SendToRecycleBin (not a hard delete) — recoverable by design.
Add-Type -AssemblyName Microsoft.VisualBasic
[Microsoft.VisualBasic.FileIO.FileSystem]::DeleteFile(
    $Path,
    [Microsoft.VisualBasic.FileIO.UIOption]::OnlyErrorDialogs,
    [Microsoft.VisualBasic.FileIO.RecycleOption]::SendToRecycleBin
)

Write-Output "recycled"
exit 0
