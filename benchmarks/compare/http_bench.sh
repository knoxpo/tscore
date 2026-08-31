#!/bin/bash
# HTTP hello-world req/s: tscore vs node vs bun, via wrk.
#
# Reports TWO rows, because they answer different questions:
#   - single-threaded: per-core engine throughput. This is the honest
#     head-to-head — Bun.serve is a single-threaded event loop by
#     default, so comparing it against a multi-worker tscore would be
#     measuring worker count, not the engine.
#   - multi-worker: scale-out. On a loopback benchmark this adds little
#     for any engine (the client shares the box), so read it as a
#     ceiling check, not a speedup.
#
# Not part of run_compare.sh (needs wrk + long-lived servers).
# Usage: benchmarks/compare/http_bench.sh [workers]
set -u
W="${1:-8}"
BIN="${TSCORE:-./target/release/tscore}"
TMP="${TMPDIR:-/tmp}/tscore-httpbench-$$"
mkdir -p "$TMP"
command -v wrk >/dev/null || { echo "wrk not installed (brew install wrk)"; exit 1; }

# 2 client threads, not 8: wrk shares this box with the server, and
# starving the server of cores measures the load generator, not the
# engine (t8/c128 reads ~20% low for every engine).
bench() { wrk -t2 -c32 -d5s "$1" 2>&1 | awk '/Requests\/sec/{print $2}'; }

cat > "$TMP/one.ts" <<EOF
await runtime.http.serve(41920, () => "hello world", { workers: 1 });
EOF
cat > "$TMP/one.mjs" <<'EOF'
import { createServer } from "node:http";
createServer((req, res) => res.end("hello world")).listen(41921, "127.0.0.1");
EOF
cat > "$TMP/one.bun.ts" <<'EOF'
Bun.serve({ port: 41922, hostname: "127.0.0.1", fetch() { return new Response("hello world"); } });
EOF
"$BIN" run "$TMP/one.ts" --no-stats-export >/dev/null 2>&1 & T=$!
node "$TMP/one.mjs" >/dev/null 2>&1 & N=$!
bun "$TMP/one.bun.ts" >/dev/null 2>&1 & B=$!
sleep 1.5
TS1=$(bench http://127.0.0.1:41920/)
ND1=$(bench http://127.0.0.1:41921/)
BN1=$(bench http://127.0.0.1:41922/)
kill $T $N $B 2>/dev/null; pkill -f "$TMP/one.mjs" 2>/dev/null; sleep 0.5

cat > "$TMP/many.ts" <<EOF
await runtime.http.serve(41890, () => "hello world", { workers: $W });
EOF
cat > "$TMP/many.mjs" <<EOF
import { createServer } from "node:http";
import cluster from "node:cluster";
if (cluster.isPrimary) { for (let i = 0; i < $W; i++) cluster.fork(); }
else createServer((req, res) => res.end("hello world")).listen(41891, "127.0.0.1");
EOF
"$BIN" run "$TMP/many.ts" --no-stats-export >/dev/null 2>&1 & T=$!
node "$TMP/many.mjs" >/dev/null 2>&1 & N=$!
sleep 1.5
TSM=$(bench http://127.0.0.1:41890/)
NDM=$(bench http://127.0.0.1:41891/)
kill $T $N 2>/dev/null; pkill -f "$TMP/many.mjs" 2>/dev/null
rm -rf "$TMP"

python3 - "$TS1" "$ND1" "$BN1" "$TSM" "$NDM" "$W" <<'PY'
import sys
ts1, nd1, bn1, tsm, ndm = (float(x) for x in sys.argv[1:6])
w = sys.argv[6]
def row(name, v, base, mark=""):
    return f"| {name} | {v:,.0f} | {v/base:.2f}x | {mark} |"
print("\n## HTTP hello, single-threaded (wrk -t2 -c32 -d5s)\n")
print("The per-core comparison: one tscore worker, one node process,")
print("one Bun.serve event loop (its default).\n")
print("| engine | req/s | vs node | winner |")
print("|---|---|---|---|")
best1 = max(ts1, nd1, bn1)
print(row("tscore (workers:1)", ts1, nd1, "**tscore**" if best1 == ts1 else ""))
print(row("node (single process)", nd1, nd1, "**node**" if best1 == nd1 else ""))
print(row("bun (Bun.serve)", bn1, nd1, "**bun**" if best1 == bn1 else ""))
print(f"\n## HTTP hello, {w} workers (same client config)\n")
print("Scale-out on a loopback benchmark is client-bound for every")
print("engine — read this as a ceiling check, not a speedup.\n")
print("| engine | req/s | vs node | vs own 1-thread |")
print("|---|---|---|---|")
print(f"| tscore (workers:{w}) | {tsm:,.0f} | {tsm/ndm:.2f}x | {tsm/ts1:.2f}x |")
print(f"| node (cluster x{w}) | {ndm:,.0f} | 1.00x | {ndm/nd1:.2f}x |")
print(f"| bun | (single-threaded by default) | — | — |")
PY
