# PF-003: validate pulseforge counters against an independent Windows tool.
#
# Method: spawn the deterministic mixed fixture once; while pulseforge
# records it (its own process, its own sampling), PowerShell samples the same
# process through Get-Process (independent implementation of the same OS
# counters). Same-run comparison: no cross-run extrapolation.
#
# Outputs a comparison JSON on stdout. Exit 0 when memory counters agree
# within tolerance; CPU accounting is reported (not gated) because hosted
# sandboxes legitimately report zero CPU time (see docs/feasibility-results.md).
#
# Usage: pwsh tools/validate-counters.ps1 [-Root <repo root>]

param(
    [string]$Root = (Split-Path $PSScriptRoot -Parent)
)

$ErrorActionPreference = 'Stop'
$fixture = Join-Path $Root 'target/release/pf-fixture-mixed.exe'
$pulseforge = Join-Path $Root 'target/release/pulseforge.exe'
foreach ($f in @($fixture, $pulseforge)) {
    if (-not (Test-Path $f)) { throw "missing $f (run cargo build --release)" }
}

$sessionRoot = Join-Path ([System.IO.Path]::GetTempPath()) ("pf-validate-" + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $sessionRoot | Out-Null

$child = Start-Process -FilePath $fixture -ArgumentList '--ms','8000' -PassThru -RedirectStandardOutput (Join-Path $sessionRoot 'fixture.out') -RedirectStandardError (Join-Path $sessionRoot 'fixture.err')
Start-Sleep -Milliseconds 400   # let the loader settle

# pulseforge records the same live process (attach path; it never touches it).
$pf = Start-Process -FilePath $pulseforge -ArgumentList 'record','--pid',"$($child.Id)",'--duration-ms','4000','--interval-ms','250','--out',$sessionRoot -PassThru -WindowStyle Hidden

# Independent sampling loop while both run.
$psSamples = @()
$deadline = (Get-Date).AddMilliseconds(3800)
while ((Get-Date) -lt $deadline -and -not $child.HasExited) {
    $p = Get-Process -Id $child.Id -ErrorAction SilentlyContinue
    if ($p) {
        $psSamples += [pscustomobject]@{
            ws       = $p.WorkingSet64
            pm       = $p.PrivateMemorySize64
            cpu_s    = if ($p.CPU -ne $null) { [double]$p.CPU } else { -1 }
            threads  = $p.Threads.Count
        }
    }
    Start-Sleep -Milliseconds 300
}
$pf.WaitForExit() | Out-Null

$sessionDir = Get-ChildItem -Path $sessionRoot -Filter '*.pfsession' -Directory | Select-Object -First 1
if (-not $sessionDir) { throw "pulseforge produced no session in $sessionRoot" }
$report = Get-Content (Join-Path $sessionDir.FullName 'report.json') -Raw | ConvertFrom-Json

$pfWsPeak   = $report.metrics.'process.resident_bytes'.max
$pfCommitMax= $report.metrics.'process.private_commit_bytes'.max
$pfCpuMean  = $report.metrics.'process.cpu_core_pct'.mean
$pfCpuFirst = $report.metrics.'process.cpu_user_ns'.first
$pfCpuLast  = $report.metrics.'process.cpu_user_ns'.last
$pfThreadsMax = $report.metrics.'process.thread_count'.max

$psWsMax  = ($psSamples | Measure-Object -Property ws -Maximum).Maximum
$psPmMax  = ($psSamples | Measure-Object -Property pm -Maximum).Maximum
$psThrMax = ($psSamples | Measure-Object -Property threads -Maximum).Maximum
$psCpuFirst = $psSamples[0].cpu_s
$psCpuLast  = $psSamples[-1].cpu_s

function RelDiff($a, $b) {
    if ($null -eq $a -or $null -eq $b -or $a -eq 0 -and $b -eq 0) { return 0 }
    [math]::Abs($a - $b) / [math]::Max($a, $b)
}

$wsDiff = RelDiff $pfWsPeak $psWsMax
$pmDiff = RelDiff $pfCommitMax $psPmMax
$wsOk   = $wsDiff -le 0.30
$pmOk   = $pmDiff -le 0.30
$threadsOk = ($psThrMax -ge 2) -and ($pfThreadsMax -ge 1)

$out = [ordered]@{
    validation       = 'pulseforge vs Get-Process, same process, same run'
    os               = [System.Environment]::OSVersion.VersionString
    pid              = $child.Id
    ps_samples       = $psSamples.Count
    pf_samples       = $report.samples
    working_set_peak = [ordered]@{ pulseforge = $pfWsPeak; get_process = $psWsMax; rel_diff = [math]::Round($wsDiff, 4); agree = $wsOk }
    private_commit_max = [ordered]@{ pulseforge = $pfCommitMax; get_process = $psPmMax; rel_diff = [math]::Round($pmDiff, 4); agree = $pmOk }
    threads_max      = [ordered]@{ pulseforge = $pfThreadsMax; get_process = $psThrMax; agree = $threadsOk }
    cpu_accounting   = [ordered]@{
        pulseforge_core_pct_mean = $pfCpuMean
        pulseforge_user_ns_delta = ($pfCpuLast - $pfCpuFirst)
        get_process_cpu_s_delta  = [math]::Round(($psCpuLast - $psCpuFirst), 6)
        note = 'both implementations read GetProcessTimes-derived values; sandboxes may report zero for spawned processes'
    }
    session          = $sessionDir.FullName
}
$out | ConvertTo-Json -Depth 4

if (-not $child.HasExited) { Stop-Process -Id $child.Id -Force -ErrorAction SilentlyContinue }
Remove-Item -Recurse -Force $sessionRoot -ErrorAction SilentlyContinue

if ($wsOk -and $pmOk) { exit 0 } else { exit 1 }
