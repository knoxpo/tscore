#!/bin/bash
# Architecture probes: tscore vs node (V8) vs bun (JSC).
# Three questions, one probe each: fan-out cost, GC/stall pauses, payload
# crossing cost.  Emits benchmarks/compare/arch/RESULTS.md.  Not part of
# run_compare.sh: separate board, separate lock.
set -euo pipefail
cd "$(dirname "$0")/../../.."

RUNS=${RUNS:-5}
DIR=benchmarks/compare/arch
OUT=$DIR/RESULTS.md

cargo build -q --release -p tscore
TSCORE=./target/release/tscore

# median of the value following KEY (and optional second field match) over
# $RUNS runs, first run discarded as warmup
median_key() { # key [field2] -- cmd...
    local key=$1 f2=$2; shift 2
    local vals=()
    "$@" >/dev/null 2>&1 || true
    for _ in $(seq "$RUNS"); do
        vals+=("$("$@" 2>/dev/null | awk -v k="$key" -v f="$f2" '$1==k && (f=="" || $2==f) {print $NF}')")
    done
    printf '%s\n' "${vals[@]}" | sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'
}
get_key() { "$@" 2>/dev/null | awk -v k="$1" '$1==k {print $2}'; }
get_result() { "$@" 2>/dev/null | awk '/^RESULT/ {print $2}'; }
check_agree() { # name a b c
    if [ "$2" != "$3" ] || [ "$2" != "$4" ]; then
        echo "FAIL: $1 RESULT mismatch (tscore=$2 node=$3 bun=$4)" >&2; exit 1
    fi
}
ratio() { awk -v a="$1" -v b="$2" 'BEGIN {printf "%.2f", a/b}'; }
winner() { awk -v t="$1" -v n="$2" -v b="$3" 'BEGIN { if (t<=n && t<=b) print "tscore"; else if (n<=b) print "node"; else print "bun" }'; }
row() { # label tscore node bun [unit-fmt]
    local fmt=${5:-%.2f} w; w=$(winner "$2" "$3" "$4")
    printf "| %s | $fmt (%sx) | $fmt (1.00x) | $fmt (%sx) | **%s** |\n" \
        "$1" "$2" "$(ratio "$2" "$3")" "$3" "$4" "$(ratio "$4" "$3")" "$w" >>"$OUT"
}

W=$(sysctl -n hw.ncpu)
{
    echo "# Architecture probes: tscore vs node (V8) vs bun (JSC)"
    echo
    echo "- date: $(date '+%Y-%m-%d %H:%M')"
    echo "- machine: $(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m), $W logical cpus ($(sysctl -n hw.perflevel0.logicalcpu 2>/dev/null || echo '?')P + $(sysctl -n hw.perflevel1.logicalcpu 2>/dev/null || echo '?')E)"
    echo "- tscore: $($TSCORE --version), node: $(node --version), bun: $(bun --version)"
    echo "- cells: median of $RUNS runs, first run discarded; pools/workers created and warmed before timing"
    echo "- ratios: engine / node — lower is better"
    echo "- node/bun variants use worker_threads + postMessage, the idiomatic way to use other cores there"
    echo
} >"$OUT"

# ---- fan-out ----
echo "== fanout ==" >&2
f=$DIR/fanout.ts; m=$DIR/fanout_workers.mjs
check_agree fanout "$(get_result "$TSCORE" run "$f")" "$(get_result node "$m")" "$(get_result bun "$m")"
{
    echo "## Fan-out: hand N trivial items to other cores and collect results (ms per batch, $W workers)"
    echo
    echo "| items | tscore parallel.map | node worker pool | bun worker pool | winner |"
    echo "|---|---|---|---|---|"
} >>"$OUT"
for n in 8 64 512 4096; do
    row "$n" "$(median_key FANOUT "$n" "$TSCORE" run "$f")" "$(median_key FANOUT "$n" node "$m")" "$(median_key FANOUT "$n" bun "$m")" "%.3f"
done

# ---- stall ----
echo "== stall (5s x 3 engines x $RUNS) ==" >&2
f=$DIR/stall.ts
{
    echo
    echo "## Pause detector: 1M-object live set + 5 s allocation churn (ms; same file on all engines)"
    echo
    echo "| metric | tscore | node | bun | winner |"
    echo "|---|---|---|---|---|"
} >>"$OUT"
for k in MAX_STALL_MS P99_MS P999_MS; do
    row "$k" "$(median_key $k "" "$TSCORE" run "$f")" "$(median_key $k "" node "$f")" "$(median_key $k "" bun "$f")" "%.2f"
done
{
    echo "| iterations in 5 s (higher is better) | $(median_key ITERS "" "$TSCORE" run "$f") | $(median_key ITERS "" node "$f") | $(median_key ITERS "" bun "$f") | |"
} >>"$OUT"

# ---- payload ----
echo "== payload ==" >&2
f=$DIR/payload.ts; m=$DIR/payload_workers.mjs
check_agree payload "$(get_result "$TSCORE" run "$f")" "$(get_result node "$m")" "$(get_result bun "$m")"
{
    echo
    echo "## Payload: round-trip a nested object to another realm/thread and back (ms per round trip)"
    echo
    echo "| items (~bytes) | tscore parallel.map | node postMessage | bun postMessage | winner |"
    echo "|---|---|---|---|---|"
} >>"$OUT"
for n in 10 1000 100000; do
    case $n in 10) lbl="10 (~1 KB)";; 1000) lbl="1,000 (~100 KB)";; *) lbl="100,000 (~10 MB)";; esac
    row "$lbl" "$(median_key PAYLOAD "$n" "$TSCORE" run "$f")" "$(median_key PAYLOAD "$n" node "$m")" "$(median_key PAYLOAD "$n" bun "$m")" "%.3f"
done
echo "wrote $OUT" >&2
