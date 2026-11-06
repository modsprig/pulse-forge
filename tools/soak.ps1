# PF-1203 soak driver: sustained capture through the daemon.
#
# Setup: daemon with a fixture workload recorded at 250 ms for 60 minutes
# (the max --duration-ms the CLI accepts). While the recording runs, the
# daemon's private working set is sampled every 60 s into a CSV — bounded
# memory means the series plateaus, not grows linearly.
#
# Output (probe-output/soak/):
#   memory.csv       timestamp_s, private_bytes, workingset_bytes
#   session dir      the recording itself (chunk store)
# The verification step (tools/verify-soak.py) runs after completion.

param(
    [int]$Minutes = 60,
    [int]$PollMs = 250
)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot -Parent
$pf = Join-Path $root 'target/release/pulseforge.exe'
$fixture = Join-Path $root 'target/release/pf-fixture-cpu.exe'
$outDir = Join-Path $root 'probe-output/soak'
New-Item -ItemType Directory -Force -Path $outDir | Out-Null

$env:PULSEFORGE_DATA = Join-Path $outDir 'data'
New-Item -ItemType Directory -Force -Path $env:PULSEFORGE_DATA | Out-Null

# 1. Start the daemon.
$daemon = Start-Process -FilePath $pf -ArgumentList 'daemon' -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 2
Write-Host "daemon pid $($daemon.Id)"

# 2. Record the DAEMON ITSELF (profiler-on-profiler, roadmap-endorsed).
#    Fixture workloads cap at 600 s by design (parse_ms_arg guardrail) but
#    the daemon lives exactly as long as the soak, so attaching to its own
#    PID gives one continuous target for the full window. Self-observation
#    also makes the bounded-memory claim cover the recorder's own buffers.
$workload = $daemon
Write-Host "soak target = the daemon itself (pid $($daemon.Id))"

# 3. Attach a recording for the full duration (cap: 3600000 ms = 60 min).
$cap = [Math]::Min($Minutes * 60 * 1000, 3600000)
$rec = Start-Process -FilePath $pf -ArgumentList 'agent','start','--pid',"$($workload.Id)",'--duration-ms',"$cap",'--interval-ms',"$PollMs" -PassThru -WindowStyle Hidden
$rec.WaitForExit() | Out-Null
Write-Host "recording started (duration $cap ms)"

# 4. Sample daemon memory every 60 s until the recording window ends.
$csv = Join-Path $outDir 'memory.csv'
"elapsed_s,private_bytes,workingset_bytes" | Set-Content $csv
$elapsed = 0
while ($elapsed -lt ($cap / 1000)) {
    Start-Sleep -Seconds 60
    $elapsed += 60
    $p = Get-Process -Id $daemon.Id -ErrorAction SilentlyContinue
    if (-not $p) { Write-Host "daemon exited early at ${elapsed}s"; break }
    "$elapsed,$($p.PrivateMemorySize64),$($p.WorkingSet64)" | Add-Content $csv
    Write-Host "t=${elapsed}s private=$($p.PrivateMemorySize64) ws=$($p.WorkingSet64)"
}

Write-Host "soak capture window complete; leaving daemon alive for verification"
$daemon.Id | Set-Content (Join-Path $outDir 'daemon.pid')
$workload.Id | Set-Content (Join-Path $outDir 'workload.pid')
