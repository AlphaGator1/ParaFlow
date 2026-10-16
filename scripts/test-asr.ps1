<#
.SYNOPSIS
    Exercises the whisper-server sidecar exactly the way src-tauri/src/asr.rs does.

.DESCRIPTION
    asr.rs depends on a handful of things that live outside the Rust type
    system: the CLI flags whisper-server accepts, the multipart field name it
    expects, and the shape of the JSON it returns. This script pins all three,
    so an upstream whisper.cpp bump that changes any of them fails here rather
    than at runtime in the middle of a dictation.

    Also reports transcription wall time, which is the number that decides
    whether the app feels instant.
#>
[CmdletBinding()]
param(
    [string]$Model = "ggml-large-v3-turbo.bin",
    [int]$Port = 8188,   # clear of the app's 8178 (speech) and 8179 (cleanup), and test-parakeet's 8187, so all can run
    [switch]$Cpu
)

$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent

$build  = if ($Cpu) { "whisper-cpu" } else { "whisper-cuda" }
$server = Join-Path $root "vendor\$build\Release\whisper-server.exe"
$mdl    = Join-Path $root "models\$Model"
$sample = Join-Path $root "models\jfk.wav"

foreach ($p in @($server, $mdl)) {
    if (-not (Test-Path $p)) { throw "missing: $p  (run scripts/setup.ps1)" }
}

if (-not (Test-Path $sample)) {
    Write-Host "Fetching speech sample..." -ForegroundColor Cyan
    $ProgressPreference = "SilentlyContinue"
    Invoke-WebRequest "https://raw.githubusercontent.com/ggml-org/whisper.cpp/master/samples/jfk.wav" -OutFile $sample
}

# Same flags asr.rs passes. Note the explicit quoting: Start-Process does not
# quote -ArgumentList entries, and this repo's own path contains spaces, so an
# unquoted model path arrives at whisper-server truncated at the first space.
# (Rust's std::process::Command quotes correctly, so asr.rs needs no such care.)
$serverArgs = @(
    "-m", "`"$mdl`"", "--host", "127.0.0.1", "--port", "$Port",
    "-t", "8", "-l", "en",
    "--no-timestamps", "--suppress-nst", "--no-fallback"
)
if ($Cpu) { $serverArgs += "--no-gpu" }

Write-Host "Starting sidecar ($build)..." -ForegroundColor Cyan
$stderrLog = Join-Path $env:TEMP "paraflow-test-asr.err.txt"
$proc = Start-Process -FilePath $server -ArgumentList $serverArgs `
    -WorkingDirectory (Split-Path $server -Parent) `
    -PassThru -WindowStyle Hidden -RedirectStandardError $stderrLog

try {
    # Wait for the listener, the same way sidecar.rs::await_ready does: any
    # HTTP answer except 503, which some servers send while still loading.
    $loadSw = [System.Diagnostics.Stopwatch]::StartNew()
    $ready = $false
    while ($loadSw.Elapsed.TotalSeconds -lt 120) {
        if ($proc.HasExited) {
            $why = if (Test-Path $stderrLog) { (Get-Content $stderrLog -Tail 5) -join "`n" } else { "(no stderr)" }
            throw "sidecar exited during startup:`n$why"
        }
        try {
            $r = Invoke-WebRequest "http://127.0.0.1:$Port/" -TimeoutSec 1 -UseBasicParsing
            if ($r.StatusCode -ne 503) { $ready = $true; break }
        } catch {
            if ($_.Exception.Response -and [int]$_.Exception.Response.StatusCode -ne 503) { $ready = $true; break }
        }
        Start-Sleep -Milliseconds 150
    }
    if (-not $ready) { throw "sidecar never became ready" }
    Write-Host ("  ready in {0} ms" -f [int]$loadSw.ElapsedMilliseconds) -ForegroundColor DarkGray

    Add-Type -AssemblyName System.Net.Http
    $client = New-Object System.Net.Http.HttpClient
    $bytes = [System.IO.File]::ReadAllBytes($sample)
    $audioSec = ([System.IO.FileInfo]$sample).Length / (16000 * 2)

    function Invoke-Inference {
        $form = New-Object System.Net.Http.MultipartFormDataContent
        $fileContent = New-Object System.Net.Http.ByteArrayContent(, $script:bytes)
        $fileContent.Headers.ContentType =
            [System.Net.Http.Headers.MediaTypeHeaderValue]::Parse("audio/wav")
        # Field name must be "file" - this is the contract asr.rs relies on.
        $form.Add($fileContent, "file", "utterance.wav")
        $form.Add((New-Object System.Net.Http.StringContent("json")), "response_format")
        $form.Add((New-Object System.Net.Http.StringContent("0.0")), "temperature")

        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        $resp = $script:client.PostAsync("http://127.0.0.1:$script:Port/inference", $form).Result
        $body = $resp.Content.ReadAsStringAsync().Result
        $sw.Stop()
        [pscustomobject]@{ Ok = $resp.IsSuccessStatusCode; Status = [int]$resp.StatusCode
                           Body = $body; Ms = [int]$sw.ElapsedMilliseconds }
    }

    # Run several times. The first call on an RTX 50-series card pays for the
    # driver JIT-compiling the shipped sm_90 PTX into sm_120 code, so run 1 is
    # not representative - which is exactly why asr.rs warms up at startup.
    $runs = @()
    foreach ($i in 1..4) { $runs += Invoke-Inference }

    $first = $runs[0]
    Write-Host "`nHTTP $($first.Status)" -ForegroundColor White
    if (-not $first.Ok) { throw "inference failed: $($first.Body)" }

    $json = $first.Body | ConvertFrom-Json
    if ($null -eq $json.text) { throw "response has no 'text' field: $($first.Body)" }

    Write-Host ("transcript : {0}" -f $json.text.Trim()) -ForegroundColor Green
    Write-Host ("audio      : {0:N1} s" -f $audioSec) -ForegroundColor DarkGray
    Write-Host "`nlatency by run (ms)" -ForegroundColor White
    for ($i = 0; $i -lt $runs.Count; $i++) {
        $tag = if ($i -eq 0) { "cold (PTX JIT)" } else { "warm" }
        Write-Host ("  run {0}  {1,6}   {2}" -f ($i + 1), $runs[$i].Ms, $tag) -ForegroundColor DarkGray
    }
    $warm = ($runs[1..($runs.Count - 1)] | Measure-Object -Property Ms -Average).Average
    Write-Host ("`nwarm mean  : {0} ms  ({1:N0}x realtime)" -f [int]$warm, ($audioSec * 1000 / $warm)) -ForegroundColor Cyan
    Write-Host "Contract OK" -ForegroundColor Green
}
finally {
    if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force }
}
