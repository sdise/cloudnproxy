# Sample resource usage of a process into a CSV (Windows).
#
# CPU is derived by differencing the process's cumulative CPU time, which is far
# steadier than an instantaneous reading:
#   cpuPercent = dCPU / dWall / logicalCores * 100
#
# Usage:
#   powershell -File sample.ps1 -Name t5d -Out out.csv -Seconds 60 -IntervalMs 500
#
# Output is ASCII on purpose: Windows PowerShell 5.1 reads non-BOM script files
# using the local ANSI codepage, so non-ASCII literals would come out garbled.

param(
    [string]$Name = 't5d',
    [string]$Out = 'sample.csv',
    [int]$Seconds = 60,
    [int]$IntervalMs = 500
)

$cores = [Environment]::ProcessorCount
$rows = New-Object System.Collections.Generic.List[string]
$rows.Add('t,workingSetMB,privateMB,cpuPercent,threads,handles')

$sw = [System.Diagnostics.Stopwatch]::StartNew()
$prevCpu = $null
$prevAt = 0.0

while ($sw.Elapsed.TotalSeconds -lt $Seconds) {
    $p = Get-Process -Name $Name -ErrorAction SilentlyContinue | Select-Object -First 1
    if ($null -eq $p) {
        Start-Sleep -Milliseconds $IntervalMs
        continue
    }

    $t = [math]::Round($sw.Elapsed.TotalSeconds, 2)
    $cpu = $p.TotalProcessorTime.TotalSeconds

    if ($null -ne $prevCpu -and ($t - $prevAt) -gt 0) {
        $pct = [math]::Round(($cpu - $prevCpu) / ($t - $prevAt) / $cores * 100, 2)
    } else {
        $pct = 0
    }

    $ws = [math]::Round($p.WorkingSet64 / 1MB, 2)
    $pv = [math]::Round($p.PrivateMemorySize64 / 1MB, 2)

    # Deliberately no TCP connection count here: Get-NetTCPConnection enumerates the
    # whole system table, taking seconds once connections reach four digits. That both
    # cripples the sampling rate and loads the very process being measured.
    $rows.Add((@($t, $ws, $pv, $pct, $p.Threads.Count, $p.HandleCount) -join ','))

    $prevCpu = $cpu
    $prevAt = $t
    Start-Sleep -Milliseconds $IntervalMs
}

$rows | Set-Content -Path $Out -Encoding UTF8
Write-Output "wrote $Out ($($rows.Count - 1) samples, $cores cores)"
