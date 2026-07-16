# Downloads the speech-recognition model Nyxa needs (Parakeet TDT 0.6B v2,
# int8 ONNX, ~640 MB) into bench/models/, where the engine's fallback
# resolver finds it when you run from the repo root.
#
# The wake-word, VAD, and hand-tracking models are small and ship in the
# repo already (models/ and crates/jarvis-app/ui/vendor/) — this script is
# only for the big ASR model.
#
# Usage:  powershell -ExecutionPolicy Bypass -File scripts\fetch_models.ps1
$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$dest = Join-Path $repoRoot 'bench\models'
$dirName = 'sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8'
$archive = "$dirName.tar.bz2"
$url = "https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/$archive"

if (Test-Path (Join-Path $dest $dirName)) {
    Write-Host "Already present: $dest\$dirName — nothing to do."
    exit 0
}

New-Item -ItemType Directory -Force $dest | Out-Null
$archivePath = Join-Path $dest $archive

Write-Host "Downloading $archive (~640 MB)..."
curl.exe -L --fail -o $archivePath $url

Write-Host "Extracting..."
tar -xjf $archivePath -C $dest
Remove-Item $archivePath

if (Test-Path (Join-Path $dest $dirName)) {
    Write-Host "Done. ASR model installed at $dest\$dirName"
} else {
    Write-Error "Extraction finished but $dirName was not found — check $dest manually."
}
