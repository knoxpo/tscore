#!/bin/bash
# M1 benchmark runner: tscore scaling curve + absolute reference vs Node.
# Emits the speedup table and asserts the M1 targets (2c>=1.7x 4c>=3x 8c>=5.5x).
set -euo pipefail
cd "$(dirname "$0")/.."

RUNS=${RUNS:-5}
WORKLOADS=(primes fnv mandelbrot)
CORESETS=(1 2 4 8)

cargo build -q --release -p tscore
TSCORE=./target/release/tscore

median_time() { # cmd... -> median TIME_MS over $RUNS runs (first run discarded as warmup)
    local times=()
    "$@" >/dev/null 2>&1 || true # warmup
    for _ in $(seq "$RUNS"); do
        local out t
        out=$("$@")
        t=$(echo "$out" | awk '/TIME_MS/ {print $2}')
        times+=("$t")
    done
    printf '%s\n' "${times[@]}" | sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'
}

check_result() { # workload cmd... : verify RESULT matches node serial
    local wl=$1; shift
    local expect got
    expect=$(node "benchmarks/programs/node/$wl.mjs" | awk '/RESULT/ {print $2}')
    got=$("$@" | awk '/RESULT/ {print $2}')
    if [ "$expect" != "$got" ]; then
        echo "FAIL: $wl result mismatch (tscore=$got node=$expect)" >&2
        exit 1
    fi
}

echo "machine: $(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m), $(sysctl -n hw.ncpu 2>/dev/null || nproc) logical cpus"
echo "runs per cell: $RUNS (median), first run discarded"
echo

fail=0
for wl in "${WORKLOADS[@]}"; do
    check_result "$wl" "$TSCORE" run "benchmarks/programs/$wl.ts" --workers 2
    node_ms=$(median_time node "benchmarks/programs/node/$wl.mjs")
    base_ms=$(median_time "$TSCORE" run "benchmarks/programs/$wl.ts" --workers 1)
    echo "== $wl =="
    printf "  %-22s %10.1f ms\n" "node (serial)" "$node_ms"
    printf "  %-22s %10.1f ms   speedup 1.00x\n" "tscore --workers 1" "$base_ms"
    for w in "${CORESETS[@]:1}"; do
        ms=$(median_time "$TSCORE" run "benchmarks/programs/$wl.ts" --workers "$w")
        speedup=$(echo "$base_ms $ms" | awk '{printf "%.2f", $1/$2}')
        printf "  %-22s %10.1f ms   speedup %sx\n" "tscore --workers $w" "$ms" "$speedup"
        case $w in 2) target=1.7 ;; 4) target=3.0 ;; *) target=5.5 ;; esac
        ok=$(echo "$speedup $target" | awk '{print ($1>=$2)?"ok":"MISS"}')
        if [ "$ok" = "MISS" ]; then
            echo "  ^^ TARGET MISS: ${w}c needs >=${target}x" >&2
            fail=1
        fi
    done
    if [ "$wl" = "primes" ] && command -v node >/dev/null; then
        wms=$(WORKERS=8 median_time node benchmarks/programs/node/primes_workers.mjs)
        printf "  %-22s %10.1f ms   (ergonomics baseline)\n" "node worker_threads x8" "$wms"
    fi
    echo
done
# Go / Rust ports pending — see docs/architecture/benchmark-methodology.md
[ $fail -eq 0 ] && echo "ALL M1 SCALING TARGETS MET" || { echo "SCALING TARGETS MISSED"; exit 1; }
