<#
.SYNOPSIS
    Benchmarks NVIDIA Parakeet TDT via parakeet.cpp, using the same method as
    scripts/test-asr.ps1 so the numbers are directly comparable.

.DESCRIPTION
    parakeet-server exposes the OpenAI transcription API
    (POST /v1/audio/transcriptions) rather than whisper-server's /inference,
    but is otherwise the same shape of sidecar: one resident model, one HTTP
    round trip per utterance.

    Reports cold and warm latency separately for the same reason test-asr.ps1
    does - the first call on an RTX 50-series card pays for driver PTX JIT and
    is not representative.
#>
[CmdletBinding()]
param(
    [string]$Model = "tdt-0.6b-v3-f16.gguf",
    [int]$Port = 8187,
    [int]$Runs = 4
)

$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent

$bin = Join-Path $root "vendor\parakeet-cuda"
$server = Join-Path $bin "parakeet-server.exe"
$mdl = Join-Path $root "models\$Model"
$sample = Join-Path $root "models\jfk.wav"

foreach ($p in @($server, $mdl)) {
    if (-not (Test-Path $p)) { throw "missing: $p" }
}
if (-not (Test-Path $sample)) {
    $ProgressPreference = "SilentlyContinue"
    Invoke-WebRequest "https://raw.githubusercontent.com/ggml-org/whisper.cpp/master/samples/jfk.wav" -OutFile $sample
}

$sizeMb = [math]::Round((Get-Item $mdl).Length / 1MB, 0)
Write-Host "model : $Model ($sizeMb MB)" -ForegroundColor White

# Quoted: this repo's path contains spaces (see test-asr.ps1 for the same trap).
$serverArgs = @("--model", "`"$mdl`"", "--host", "127.0.0.1", "--port", "$Port", "--threads", "8")
$errLog = Join-Path $env:TEMP "flowstate-test-parakeet.err.txt"

$proc = Start-Process -FilePath $server -ArgumentList $serverArgs -WorkingDirectory $bin `
    -PassThru -WindowStyle Hidden -RedirectStandardError $errLog

try {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $ready = $false
    while ($sw.Elapsed.TotalSeconds -lt 180) {
        if ($proc.HasExited) {
            $why = if (Test-Path $errLog) { (Get-Content $errLog -Tail 8) -join "`n" } else { "(no stderr)" }
            throw "parakeet-server exited during startup:`n$why"
        }
        try {
            $r = Invoke-WebRequest "http://127.0.0.1:$Port/" -TimeoutSec 2 -UseBasicParsing
            if ($r.StatusCode -ne 503) { $ready = $true; break }
        } catch {
            if ($_.Exception.Response -and [int]$_.Exception.Response.StatusCode -ne 503) { $ready = $true; break }
            Start-Sleep -Milliseconds 200
        }
    }
    if (-not $ready) { throw "parakeet-server never became ready" }
    $loadMs = [int]$sw.ElapsedMilliseconds
    Write-Host "  ready in $loadMs ms" -ForegroundColor DarkGray

    Add-Type -AssemblyName System.Net.Http
    $client = New-Object System.Net.Http.HttpClient
    $client.Timeout = [TimeSpan]::FromSeconds(120)
    $bytes = [System.IO.File]::ReadAllBytes($sample)
    $audioSec = ([System.IO.FileInfo]$sample).Length / (16000 * 2)

    $results = @()
    foreach ($i in 1..$Runs) {
        $form = New-Object System.Net.Http.MultipartFormDataContent
        $fc = New-Object System.Net.Http.ByteArrayContent(, $bytes)
        $fc.Headers.ContentType = [System.Net.Http.Headers.MediaTypeHeaderValue]::Parse("audio/wav")
        $form.Add($fc, "file", "utterance.wav")
        $form.Add((New-Object System.Net.Http.StringContent("json")), "response_format")

        $t = [Diagnostics.Stopwatch]::StartNew()
        $resp = $client.PostAsync("http://127.0.0.1:$Port/v1/audio/transcriptions", $form).Result
        $body = $resp.Content.ReadAsStringAsync().Result
        $t.Stop()
        if (-not $resp.IsSuccessStatusCode) { throw "inference failed ($($resp.StatusCode)): $body" }
        $results += [pscustomobject]@{ Ms = [int]$t.ElapsedMilliseconds; Body = $body }
    }

    $json = $results[0].Body | ConvertFrom-Json
    $text = if ($null -ne $json.text) { $json.text } else { $results[0].Body }

    Write-Host "`ntranscript : $($text.Trim())" -ForegroundColor Green
    Write-Host ("audio      : {0:N1} s" -f $audioSec) -ForegroundColor DarkGray
    Write-Host "`nlatency by run (ms)" -ForegroundColor White
    for ($i = 0; $i -lt $results.Count; $i++) {
        $tag = if ($i -eq 0) { "cold" } else { "warm" }
        Write-Host ("  run {0}  {1,6}   {2}" -f ($i + 1), $results[$i].Ms, $tag) -ForegroundColor DarkGray
    }
    if ($results.Count -gt 1) {
        $warm = ($results[1..($results.Count - 1)] | Measure-Object -Property Ms -Average).Average
        Write-Host ("`nwarm mean  : {0} ms  ({1:N0}x realtime)  |  load {2} ms  |  {3} MB" -f `
            [int]$warm, ($audioSec * 1000 / $warm), $loadMs, $sizeMb) -ForegroundColor Cyan
    }
}
finally {
    if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force }
}
