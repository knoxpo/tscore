#!/bin/bash
# HTTP hello-world req/s: tscore vs node (cluster) vs bun, via wrk.
# Not part of run_compare.sh (needs wrk + long-lived servers); run it
# separately and paste the row into RESULTS.md.
#
# Usage: benchmarks/compare/http_bench.sh [workers]
set -u
W="${1:-8}"
BIN="${TSCORE:-./target/release/tscore}"
TMP="${TMPDIR:-/tmp}/tscore-httpbench-$$"
mkdir -p "$TMP"
command -v wrk >/dev/null || { echo "wrk not installed (brew install wrk)"; exit 1; }

cat > "$TMP/s.ts" <<EOF
await runtime.http.serve(41890, () => "hello world", { workers: $W });
EOF
cat > "$TMP/s.mjs" <<EOF
import { createServer } from "node:http";
import cluster from "node:cluster";
if (cluster.isPrimary) { for (let i = 0; i < $W; i++) cluster.fork(); }
else createServer((req, res) => res.end("hello world")).listen(41891, "127.0.0.1");
EOF
cat > "$TMP/s.bun.ts" <<'EOF'
Bun.serve({ port: 41892, hostname: "127.0.0.1", fetch() { return new Response("hello world"); } });
EOF

"$BIN" run "$TMP/s.ts" --no-stats-export >/dev/null 2>&1 & T=$!
node "$TMP/s.mjs" >/dev/null 2>&1 & N=$!
bun "$TMP/s.bun.ts" >/dev/null 2>&1 & B=$!
sleep 1.5

# 2 client threads, not 8: wrk shares this box with the server, and
# starving the server of cores measures the load generator, not the
# engine (t8/c128 reads ~20% lower for every engine).
bench() { wrk -t2 -c32 -d5s "$1" 2>&1 | awk '/Requests\/sec/{print $2}'; }
TS=$(bench http://127.0.0.1:41890/)
ND=$(bench http://127.0.0.1:41891/)
BN=$(bench http://127.0.0.1:41892/)
kill $T $N $B 2>/dev/null
pkill -f "$TMP/s.mjs" 2>/dev/null
rm -rf "$TMP"

python3 - "$TS" "$ND" "$BN" "$W" <<'PY'
import sys
ts, nd, bn, w = float(sys.argv[1]), float(sys.argv[2]), float(sys.argv[3]), sys.argv[4]
best = max(ts, nd, bn)
win = "tscore" if best == ts else ("node" if best == nd else "bun")
print(f"\n## HTTP hello (wrk -t2 -c32 -d5s, {w} workers each)\n")
print("| engine | req/s | vs node | winner |")
print("|---|---|---|---|")
print(f"| tscore (http.serve) | {ts:,.0f} | {ts/nd:.2f}x | |")
print(f"| node (cluster) | {nd:,.0f} | 1.00x | |")
print(f"| bun (Bun.serve) | {bn:,.0f} | {bn/nd:.2f}x | **{win}** |")
PY
