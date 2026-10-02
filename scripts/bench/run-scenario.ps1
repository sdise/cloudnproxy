# Run one load scenario (Windows): mock T5 node -> t5d -> sampler -> load client.
#
# Output is ASCII on purpose - see sample.ps1 for why.
#
# Usage:
#   powershell -File run-scenario.ps1 -Name A -Conns 500 -Mode idle -MockMode idle -Seconds 60
#
# Defaults pick up target/release/t5d.exe from the repository; override with -T5d.

param(
    [string]$Name = 'A',
    [int]$Conns = 100,
    [string]$Mode = 'idle',        # load client: idle | bulk
    [string]$MockMode = 'idle',    # mock node:   idle | sink
    [int]$MockRateKb = 0,          # per-connection downstream cap (KB/s), 0 = unlimited
    [int]$Seconds = 30,
    [int]$ProxyPort = 18080,       # deliberately not 10801, to avoid clashing with a real instance
    [int]$MockPort = 19990,
    [string]$T5d = '',
    [string]$OutDir = ''
)

$ErrorActionPreference = 'Continue'
$here = $PSScriptRoot
if (-not $OutDir) { $OutDir = Join-Path $here 'out' }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

if (-not $T5d) {
    foreach ($c in @('..\..\target\release\t5d.exe', '..\..\target\debug\t5d.exe')) {
        $full = Join-Path $here $c
        if (Test-Path $full) { $T5d = (Resolve-Path $full).Path; break }
    }
}
if (-not $T5d -or -not (Test-Path $T5d)) {
    Write-Output '!! t5d executable not found.'
    Write-Output '   Build it with: cargo build --release -p t5-daemon'
    Write-Output '   or pass -T5d <path>'
    exit 1
}

$out = Join-Path $OutDir "result-$Name"

function Cleanup {
    Get-Process -Name t5d -ErrorAction SilentlyContinue |
        Stop-Process -Force -ErrorAction SilentlyContinue
    Get-CimInstance Win32_Process -Filter "Name='node.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandLine -like '*mock-t5.js*' -or $_.CommandLine -like '*bench.js*' } |
        ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
}

Cleanup
Start-Sleep -Milliseconds 800

$busy = (Get-NetTCPConnection -LocalPort $ProxyPort -State Listen -ErrorAction SilentlyContinue |
         Measure-Object).Count
if ($busy -gt 0) {
    Write-Output "!! port $ProxyPort is already in use - abort"
    exit 1
}

# Config: tunnel pool and reconnect are off so pre-built connections cannot skew the
# numbers, and egress binding follows the system routing table because the target is
# on loopback (binding a physical NIC there would be wrong).
$cfg = Join-Path $OutDir "config-$Name.toml"
@"
listen_host = "127.0.0.1"
listen_port = $ProxyPort
allow_lan = false
resolve_domain = ""
upstream = "127.0.0.1:$MockPort"
current_node = "127.0.0.1:$MockPort"
fake_host = "cloudnproxy.baidu.com"
t5_auth = "bench"
max_conns = 0
chain_enabled = false
egress_interface = "system"
connect_timeout_ms = 5000
tcp_nodelay = true
tunnel_pool = false
auto_reconnect = false
auto_switch = false
log_level = "warn"
log_file = ""
"@ | Set-Content -Path $cfg -Encoding UTF8

Write-Output "=== scenario $Name : conns=$Conns client=$Mode mock=$MockMode rate=${MockRateKb}KB/s secs=$Seconds ==="
Write-Output "t5d: $T5d"

$mock = Start-Process node -ArgumentList "$here\mock-t5.js", "$MockPort", $MockMode, $MockRateKb `
    -RedirectStandardOutput "$out.mock.log" -RedirectStandardError "$out.mock.err" `
    -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 1

$t5d = Start-Process $T5d -ArgumentList '-f', $cfg, '-log', 'warn' `
    -RedirectStandardOutput "$out.t5d.log" -RedirectStandardError "$out.t5d.err" `
    -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 2

if ($t5d.HasExited) {
    Write-Output '!! t5d failed to start'
    Get-Content "$out.t5d.log" -ErrorAction SilentlyContinue | Select-Object -First 20
    Cleanup
    exit 1
}
Write-Output "t5d started pid=$($t5d.Id)"

$sampler = Start-Process powershell -ArgumentList '-NoProfile', '-File', "$here\sample.ps1", `
    '-Name', 't5d', '-Out', "$out.csv", '-Seconds', $Seconds, '-IntervalMs', '500' `
    -PassThru -WindowStyle Hidden
Start-Sleep -Seconds 2

& node "$here\bench.js" $ProxyPort $Conns $Mode $Seconds 2>&1 |
    Tee-Object -FilePath "$out.bench.log" | Select-Object -Last 3

$sampler.WaitForExit()
Cleanup
Start-Sleep -Milliseconds 500

Write-Output "=== $Name done -> $out.csv ==="
