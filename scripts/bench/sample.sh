#!/usr/bin/env bash
#
# Sample resource usage of a process into a CSV (Linux).
#
# CPU is derived by differencing utime+stime from /proc/<pid>/stat (in clock ticks),
# which is far steadier than an instantaneous reading:
#   cpuPercent = dCPU / dWall / logicalCores * 100
#
# Usage:
#   sample.sh [process-name] [out.csv] [seconds] [intervalMs]

set -u

NAME="${1:-t5d}"
OUT="${2:-sample.csv}"
DURATION="${3:-60}"
INTERVAL_MS="${4:-500}"

CORES="$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc 2>/dev/null || echo 1)"
CLK_TCK="$(getconf CLK_TCK 2>/dev/null || echo 100)"
INTERVAL_S="$(awk -v ms="$INTERVAL_MS" 'BEGIN{printf "%.3f", ms/1000}')"

echo 't,workingSetMB,privateMB,cpuPercent,threads,handles' > "$OUT"

find_pid() {
    pgrep -x "$NAME" 2>/dev/null | head -n1
}

start="$(date +%s.%N)"
prev_cpu=''
prev_t=0

while :; do
    now="$(date +%s.%N)"
    t="$(awk -v a="$now" -v b="$start" 'BEGIN{printf "%.2f", a-b}')"
    awk -v a="$t" -v b="$DURATION" 'BEGIN{exit !(a < b)}' || break

    pid="$(find_pid)"
    if [ -z "$pid" ]; then
        sleep "$INTERVAL_S"
        continue
    fi

    # Fields 14 and 15 of /proc/<pid>/stat are utime and stime, in clock ticks
    read -r utime stime < <(awk '{print $14, $15}' "/proc/$pid/stat" 2>/dev/null)
    if [ -z "${utime:-}" ]; then
        sleep "$INTERVAL_S"
        continue
    fi

    cpu_sec="$(awk -v c="$((utime + stime))" -v h="$CLK_TCK" 'BEGIN{printf "%.4f", c/h}')"

    pct=0
    if [ -n "$prev_cpu" ]; then
        pct="$(awk -v c="$cpu_sec" -v p="$prev_cpu" -v t="$t" -v pt="$prev_t" -v n="$CORES" \
            'BEGIN{ d = t - pt; if (d > 0) printf "%.2f", (c-p)/d/n*100; else print 0 }')"
    fi

    # smaps_rollup gives a real private-memory figure; fall back to VmRSS if unavailable
    priv_kb="$(awk '/^Private_Clean:|^Private_Dirty:/{s+=$2} END{if (s>0) printf "%.0f", s}' \
        "/proc/$pid/smaps_rollup" 2>/dev/null)"
    if [ -z "$priv_kb" ] || [ "$priv_kb" = "0" ]; then
        priv_kb="$(awk '/^VmRSS:/{print $2}' "/proc/$pid/status" 2>/dev/null)"
    fi

    rss_kb="$(awk '/^VmRSS:/{print $2}' "/proc/$pid/status" 2>/dev/null)"
    threads="$(awk '/^Threads:/{print $2}' "/proc/$pid/status" 2>/dev/null)"
    fds="$(ls "/proc/$pid/fd" 2>/dev/null | wc -l)"

    ws="$(awk -v k="${rss_kb:-0}" 'BEGIN{printf "%.2f", k/1024}')"
    pv="$(awk -v k="${priv_kb:-0}" 'BEGIN{printf "%.2f", k/1024}')"

    echo "$t,$ws,$pv,$pct,${threads:-0},${fds:-0}" >> "$OUT"

    prev_cpu="$cpu_sec"
    prev_t="$t"
    sleep "$INTERVAL_S"
done

echo "wrote $OUT ($(($(wc -l < "$OUT") - 1)) samples, $CORES cores)"
