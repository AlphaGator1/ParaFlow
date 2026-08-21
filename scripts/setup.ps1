<#
.SYNOPSIS
    Fetches the inference runtime and model weights ParaFlow needs at runtime.

.DESCRIPTION
    vendor/ and models/ are deliberately not in version control - together they
    are ~2.5 GB. This script reproduces them from upstream releases.

    The CUDA build is published against CUDA 12.4, which predates Blackwell
    (sm_120). It still runs correctly on RTX 50-series because the driver
    JIT-compiles the shipped sm_90 PTX. Verify with scripts/bench.ps1 after any
    upstream bump - a build that drops PTX in favour of arch-specific cubins
    only would break silently on these cards.
#>
[CmdletBinding()]
param(
    # whisper.cpp nightly tag providing the Windows binaries.
    [string]$WhisperTag = "b4938",
    # llama.cpp release providing llama-server for the cleanup pass.
    [string]$LlamaTag = "b10539",
    # parakeet.cpp release providing the default ASR backend.
    [string]$ParakeetTag = "v0.5.0",
    # Skip the 1.6 GB quality model and use base.en only.
    [switch]$SmallOnly,
    # Skip the cleanup model; dictation still works, pasting raw transcripts.
    [switch]$NoCleanup
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$root   = Split-Path $PSScriptRoot -Parent
$vendor = Join-Path $root "vendor"
$models = Join-Path $root "models"
New-Item -ItemType Directory -Force $vendor, $models | Out-Null

function Get-IfMissing {
    param([string]$Uri, [string]$Path, [string]$Label)
    if (Test-Path $Path) {
        Write-Host "  [skip] $Label already present" -ForegroundColor DarkGray
        return
    }
    Write-Host "  [get ] $Label" -ForegroundColor Cyan
    Invoke-WebRequest -Uri $Uri -OutFile $Path
    $mb = [math]::Round((Get-Item $Path).Length / 1MB, 1)
    Write-Host "         $mb MB" -ForegroundColor DarkGray
}

# --- Parakeet: the default ASR backend -------------------------------------
# 2.2x faster than whisper large-v3-turbo and 42% smaller, with an identical
# transcript on the benchmark sample. parakeet-server speaks the OpenAI
# transcription API, so it is a drop-in for the whisper-server sidecar.
Write-Host "`nASR runtime (Parakeet, default)" -ForegroundColor White
$pbase = "https://github.com/mudler/parakeet.cpp/releases/download/$ParakeetTag"
Get-IfMissing "$pbase/parakeet-$ParakeetTag-bin-win-cuda-x64.zip" "$vendor\parakeet.zip" "parakeet.cpp (CUDA)"
Get-IfMissing "$pbase/cudart-parakeet-bin-win-cuda-x64.zip" "$vendor\parakeet-rt.zip" "parakeet CUDA runtime"

if (-not (Test-Path "$vendor\parakeet-cuda\parakeet-server.exe")) {
    $stage = Join-Path $env:TEMP "paraflow-parakeet-stage"
    Remove-Item $stage -Recurse -Force -ErrorAction SilentlyContinue
    Expand-Archive "$vendor\parakeet.zip" $stage -Force
    Expand-Archive "$vendor\parakeet-rt.zip" $stage -Force
    New-Item -ItemType Directory -Force "$vendor\parakeet-cuda" | Out-Null
    # Both archives nest their contents one directory deep, and the CUDA DLLs
    # arrive in a separate archive from the binaries. Flatten the two together
    # so the exe can find its DLLs and the path carries no version number.
    Get-ChildItem $stage -Recurse -File | ForEach-Object {
        Copy-Item $_.FullName "$vendor\parakeet-cuda" -Force
    }
    Remove-Item $stage -Recurse -Force
}

# --- Whisper: kept selectable via PARAFLOW_ASR=whisper ---------------------
$base = "https://github.com/ggml-org/whisper.cpp/releases/download/$WhisperTag"

Write-Host "`nASR runtime (whisper, fallback)" -ForegroundColor White
Get-IfMissing "$base/whisper-cublas-12.4.0-bin-x64.zip" "$vendor\whisper-cublas.zip" "whisper.cpp (CUDA)"
Get-IfMissing "$base/whisper-blas-bin-x64.zip"          "$vendor\whisper-blas.zip"   "whisper.cpp (CPU fallback)"

if (-not (Test-Path "$vendor\whisper-cuda")) {
    Expand-Archive "$vendor\whisper-cublas.zip" "$vendor\whisper-cuda" -Force
}
if (-not (Test-Path "$vendor\whisper-cpu")) {
    Expand-Archive "$vendor\whisper-blas.zip" "$vendor\whisper-cpu" -Force
}

if (-not $NoCleanup) {
    # llama.cpp ships a CUDA 13.3 build, which supports Blackwell (sm_120)
    # natively rather than relying on PTX JIT the way the whisper.cpp binaries
    # do. Its CUDA runtime is a separate archive, so the two unzip together.
    Write-Host "`nCleanup runtime" -ForegroundColor White
    $lbase = "https://github.com/ggml-org/llama.cpp/releases/download/$LlamaTag"
    Get-IfMissing "$lbase/llama-$LlamaTag-bin-win-cuda-13.3-x64.zip" "$vendor\llama-cuda.zip" "llama.cpp (CUDA 13.3)"
    Get-IfMissing "$lbase/cudart-llama-bin-win-cuda-13.3-x64.zip" "$vendor\llama-cudart.zip" "CUDA 13.3 runtime"
    if (-not (Test-Path "$vendor\llama-cuda\llama-server.exe")) {
        Expand-Archive "$vendor\llama-cuda.zip" "$vendor\llama-cuda" -Force
        Expand-Archive "$vendor\llama-cudart.zip" "$vendor\llama-cuda" -Force
    }
}

Write-Host "`nModels" -ForegroundColor White
# Parakeet TDT 0.6B v3. q8_0 measured identical to f16 in both transcript and
# latency on the benchmark sample, at 40% less disk - so q8_0 is the default.
Get-IfMissing `
    "https://huggingface.co/mudler/parakeet-cpp-gguf/resolve/main/tdt-0.6b-v3-q8_0.gguf" `
    "$models\tdt-0.6b-v3-q8_0.gguf" "Parakeet TDT 0.6B v3 q8_0 (default ASR model)"

$hf = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main"
Get-IfMissing "$hf/ggml-base.en.bin" "$models\ggml-base.en.bin" "base.en (small install option)"
if (-not $SmallOnly) {
    Get-IfMissing "$hf/ggml-large-v3-turbo.bin" "$models\ggml-large-v3-turbo.bin" "large-v3-turbo (whisper fallback)"
}
# Silero VAD was fetched here to trim silence before the encoder. Nothing ever
# wired it up, and it stopped being worth wiring up once Parakeet brought
# transcription down to ~50 ms - so it is no longer downloaded.

if (-not $NoCleanup) {
    Get-IfMissing `
        "https://huggingface.co/unsloth/Qwen3-4B-Instruct-2507-GGUF/resolve/main/Qwen3-4B-Instruct-2507-Q4_K_M.gguf" `
        "$models\Qwen3-4B-Instruct-2507-Q4_K_M.gguf" "Qwen3-4B Instruct (cleanup pass)"
}

Write-Host "`nVerifying" -ForegroundColor White
$required = @{
    "parakeet-server" = "$vendor\parakeet-cuda\parakeet-server.exe"
    "whisper-server"  = "$vendor\whisper-cuda\Release\whisper-server.exe"
}
foreach ($k in $required.Keys) {
    if (Test-Path $required[$k]) {
        Write-Host ("  {0,-16} ok" -f $k) -ForegroundColor DarkGray
    } else {
        throw "$k missing after extraction at $($required[$k]) - the upstream archive layout may have changed."
    }
}
try {
    $gpu = (nvidia-smi --query-gpu=name,memory.total --format=csv,noheader) -join ", "
    Write-Host "  GPU: $gpu" -ForegroundColor DarkGray
} catch {
    Write-Host "  No NVIDIA GPU detected - expect substantially higher latency." -ForegroundColor Yellow
}

Write-Host "`nSetup complete. Run: npm run tauri dev`n" -ForegroundColor Green
