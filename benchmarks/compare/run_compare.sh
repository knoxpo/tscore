#!/bin/bash
# Cross-engine comparison: tscore vs node (V8) vs bun (JSC).
# Emits benchmarks/compare/RESULTS.md. No pass/fail gates — comparison, not CI.
# Shared single-file benchmarks run byte-identical on all three engines;
# concurrency benchmarks use per-engine variants (same algorithm).
set -euo pipefail
cd "$(dirname "$0")/../.."

# single-runner lock: concurrent runs interleave RESULTS.md and contend for
# CPU, corrupting both reports and numbers
LOCK=benchmarks/compare/.lock
if ! mkdir "$LOCK" 2>/dev/null; then
    echo "another run_compare.sh is already running (rm -rf $LOCK if stale)" >&2
    exit 1
fi
trap 'rmdir "$LOCK"' EXIT

RUNS=${RUNS:-5}
DIR=benchmarks/compare
GEN=$DIR/generated
OUT=$DIR/RESULTS.md

cargo build -q --release -p tscore
TSCORE=./target/release/tscore

node "$DIR/bench/gen_parse_input.mjs" "$GEN/parse_big.ts" >&2

median_time() { # cmd... -> median TIME_MS over $RUNS runs (first run discarded as warmup)
    local times=()
    "$@" >/dev/null 2>&1 || true # warmup
    for _ in $(seq "$RUNS"); do
        local t
        t=$("$@" 2>/dev/null | awk '/^TIME_MS/ {print $2}')
        times+=("$t")
    done
    printf '%s\n' "${times[@]}" | sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'
}

get_result() { "$@" 2>/dev/null | awk '/^RESULT/ {print $2}'; }

check_agree() { # name r_tscore r_node r_bun
    if [ "$2" != "$3" ] || [ "$2" != "$4" ]; then
        echo "FAIL: $1 RESULT mismatch (tscore=$2 node=$3 bun=$4)" >&2
        exit 1
    fi
}

ratio() { awk -v a="$1" -v b="$2" 'BEGIN {printf "%.2f", a/b}'; }

WINS_TSCORE=0; WINS_NODE=0; WINS_BUN=0
winner() { # tscore node bun [max] -> engine name with min (or max) value
    awk -v t="$1" -v n="$2" -v b="$3" -v m="${4:-min}" 'BEGIN {
        if (m == "max") { t = -t; n = -n; b = -b }
        if (t <= n && t <= b) print "tscore";
        else if (n <= b) print "node";
        else print "bun";
    }'
}
tally() { case $1 in tscore) WINS_TSCORE=$((WINS_TSCORE+1));; node) WINS_NODE=$((WINS_NODE+1));; bun) WINS_BUN=$((WINS_BUN+1));; esac; }

row() { # name tscore_ms node_ms bun_ms -> markdown row with ratios vs node + winner
    local w; w=$(winner "$2" "$3" "$4"); tally "$w"
    printf "| %s | %.1f (%sx) | %.1f (1.00x) | %.1f (%sx) | **%s** |\n" \
        "$1" "$2" "$(ratio "$2" "$3")" "$3" "$4" "$(ratio "$4" "$3")" "$w" >>"$OUT"
}

{
    echo "# Engine comparison: tscore vs node (V8) vs bun (JSC)"
    echo
    echo "- date: $(date '+%Y-%m-%d %H:%M')"
    echo "- machine: $(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m), $(sysctl -n hw.ncpu) logical cpus ($(sysctl -n hw.perflevel0.logicalcpu 2>/dev/null || echo '?')P + $(sysctl -n hw.perflevel1.logicalcpu 2>/dev/null || echo '?')E)"
    echo "- tscore: $($TSCORE --version), node: $(node --version), bun: $(bun --version)"
    echo "- steady-state cells: median TIME_MS of $RUNS runs, first run discarded; in-program warmup pass before timing"
    echo "- ratios: engine_ms / node_ms — lower is better, <1.00x is faster than node"
    echo "- methodology: shared benchmarks are byte-identical sources in the tscore language subset;"
    echo "  node/bun run non-idiomatic code (no Array.map, classes, etc.) — this compares engines on"
    echo "  identical programs, not idiomatic-per-engine programs"
    echo
} >"$OUT"

# ---- startup + parse/compile (hyperfine, end-to-end process time) ----
echo "== startup + parse/compile (hyperfine) ==" >&2
hf() { # file json
    hyperfine -w 3 -N --export-json "$2" \
        "$TSCORE run $1" "node $1" "bun $1" >&2
}
hf "$DIR/bench/hello.ts" "$GEN/hf_hello.json"
hf "$GEN/parse_big.ts" "$GEN/hf_parse.json"
hf_medians() { node -p 'JSON.parse(require("fs").readFileSync(process.argv[1])).results.map(r=>(r.median*1000).toFixed(1)).join(" ")' "$1"; }

{
    echo "## Startup + parse/compile (end-to-end, hyperfine)"
    echo
    echo "| benchmark | tscore | node | bun | winner |"
    echo "|---|---|---|---|---|"
} >>"$OUT"
read -r h_ts h_node h_bun <<<"$(hf_medians "$GEN/hf_hello.json")"
row "startup (hello.ts)" "$h_ts" "$h_node" "$h_bun"
read -r p_ts p_node p_bun <<<"$(hf_medians "$GEN/hf_parse.json")"
row "parse+compile (~50k LOC)" "$p_ts" "$p_node" "$p_bun"

# ---- steady-state shared benchmarks ----
{
    echo
    echo "## Steady-state (shared sources, in-program TIME_MS)"
    echo
    echo "| benchmark | tscore | node | bun | winner |"
    echo "|---|---|---|---|---|"
} >>"$OUT"
for b in objects closures alloc gc_churn promises; do
    echo "== $b ==" >&2
    f=$DIR/bench/$b.ts
    check_agree "$b" "$(get_result "$TSCORE" run "$f")" "$(get_result node "$f")" "$(get_result bun "$f")"
    row "$b" "$(median_time "$TSCORE" run "$f")" "$(median_time node "$f")" "$(median_time bun "$f")"
done

# ---- async: timers + channels (per-engine variants, same algorithm) ----
{
    echo
    echo "## Async (per-engine variants, same algorithm)"
    echo
    echo "| benchmark | tscore | node | bun | winner |"
    echo "|---|---|---|---|---|"
} >>"$OUT"
echo "== async_sleep ==" >&2
ts_f=$DIR/bench/async_sleep_tscore.ts; js_f=$DIR/bench/async_sleep_node.mjs
check_agree async_sleep "$(get_result "$TSCORE" run "$ts_f")" "$(get_result node "$js_f")" "$(get_result bun "$js_f")"
row "timer storm (2000x sleep 1ms)" "$(median_time "$TSCORE" run "$ts_f")" "$(median_time node "$js_f")" "$(median_time bun "$js_f")"

echo "== channels ==" >&2
ts_f=$DIR/bench/chan/channel_tscore.ts; js_f=$DIR/bench/chan/channel_workers.mjs
check_agree channels "$(get_result "$TSCORE" run "$ts_f")" "$(get_result node "$js_f")" "$(get_result bun "$js_f")"
row "channel 100k msgs (vs worker postMessage)" "$(median_time "$TSCORE" run "$ts_f")" "$(median_time node "$js_f")" "$(median_time bun "$js_f")"

# ---- long-running stability (single 30s run per engine) ----
echo "== longrun (30s per engine) ==" >&2
longrun() { # cmd... -> "OPS STABILITY"
    "$@" 2>/dev/null | awk '/^OPS/ {o=$2} /^STABILITY/ {s=$2} END {print o, s}'
}
f=$DIR/bench/longrun.ts
read -r lr_ts_ops lr_ts_st <<<"$(longrun "$TSCORE" run "$f")"
read -r lr_n_ops lr_n_st <<<"$(longrun node "$f")"
read -r lr_b_ops lr_b_st <<<"$(longrun bun "$f")"
lr_w=$(winner "$lr_ts_ops" "$lr_n_ops" "$lr_b_ops" max); tally "$lr_w"
{
    echo
    echo "## Long-running (30s sustained mixed compute+alloc, single run)"
    echo
    echo "| metric | tscore | node | bun | winner |"
    echo "|---|---|---|---|---|"
    echo "| throughput (ops/sec) | $lr_ts_ops ($(ratio "$lr_ts_ops" "$lr_n_ops")x) | $lr_n_ops (1.00x) | $lr_b_ops ($(ratio "$lr_b_ops" "$lr_n_ops")x) | **$lr_w** |"
    echo "| stability (last/first decile) | $lr_ts_st | $lr_n_st | $lr_b_st | |"
} >>"$OUT"

# ---- multicore scaling ----
{
    echo
    echo "## Multicore (tscore parallel.map vs node/bun worker_threads)"
    echo
} >>"$OUT"
mc() { # label tscore_prog workers_mjs
    local label=$1 ts_prog=$2 mjs=$3
    {
        echo "### $label"
        echo
        echo "| workers | tscore | node | bun | winner |"
        echo "|---|---|---|---|---|"
    } >>"$OUT"
    for w in 1 2 4 8; do
        echo "== $label x$w ==" >&2
        local t n b
        t=$(median_time "$TSCORE" run "$ts_prog" --workers "$w")
        n=$(WORKERS=$w median_time node "$mjs")
        b=$(WORKERS=$w median_time bun "$mjs")
        row "${w}" "$t" "$n" "$b"
    done
    echo >>"$OUT"
}
mc "primes" benchmarks/programs/primes.ts benchmarks/programs/node/primes_workers.mjs
mc "mandelbrot" benchmarks/programs/mandelbrot.ts "$DIR/bench/mc/mandel_workers.mjs"

# ---- scoreboard ----
{
    echo "## Scoreboard"
    echo
    echo "| engine | wins |"
    echo "|---|---|"
    echo "| tscore | $WINS_TSCORE |"
    echo "| node | $WINS_NODE |"
    echo "| bun | $WINS_BUN |"
    echo
    echo "Win = fastest median (highest throughput for long-running) on that row."
    echo
} >>"$OUT"

# ---- not benchmarkable yet ----
{
    echo "## Runtime surface"
    echo
    echo "| category | status |"
    echo "|---|---|"
    echo "| modules | SHIPPED — full ESM (static + dynamic import, cycles, bare specifiers, TLA) |"
    echo "| networking | SHIPPED — runtime.net TCP (kqueue reactor, connect) + runtime.http |"
    echo "| async file I/O | SHIPPED — runtime.fs (promise-native, dedicated I/O pool) + bytes |"
    echo "| http throughput | see the HTTP row above (benchmarks/compare/http_bench.sh) |"
    echo "| async I/O | proxied by timer-storm + channel benchmarks above |"
} >>"$OUT"

echo >&2
echo "done -> $OUT" >&2
