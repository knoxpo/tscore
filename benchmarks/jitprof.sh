#!/bin/bash
# Sample a tscore run and symbolize JIT frames.
#
# `sample` reports JIT-heap addresses as "??? (in <unknown binary>)".
# TSC_JIT_MAP makes the runtime dump "<addr> <size> <name>" per compiled
# region; this script joins the two so profiles attribute native time to
# the TypeScript function that generated it.
#
# Usage: benchmarks/jitprof.sh <program.ts> [tscore args...]
set -u
PROG="${1:?usage: jitprof.sh <program.ts> [args...]}"
shift || true
BIN="${TSCORE:-./target/release/tscore}"
TMP="${TMPDIR:-/tmp}/tscore-jitprof-$$"
mkdir -p "$TMP"
MAP="$TMP/jit.map"

TSC_JIT_MAP="$MAP" "$BIN" run "$PROG" --no-stats-export "$@" >/dev/null 2>&1 &
PID=$!
sample "$PID" 2 -mayDie > "$TMP/sample.txt" 2>/dev/null
wait $PID 2>/dev/null

python3 - "$MAP" "$TMP/sample.txt" <<'PY'
import sys, re
map_path, sample_path = sys.argv[1], sys.argv[2]
regions = []
try:
    for line in open(map_path):
        parts = line.split(None, 2)
        if len(parts) == 3:
            regions.append((int(parts[0], 16), int(parts[1], 16), parts[2].strip()))
except FileNotFoundError:
    pass
regions.sort()

def symbolize(addr):
    lo, hi = 0, len(regions) - 1
    while lo <= hi:
        mid = (lo + hi) // 2
        start, size, name = regions[mid]
        if addr < start:
            hi = mid - 1
        elif addr >= start + size:
            lo = mid + 1
        else:
            return f"{name}+{addr - start:#x}"
    return None

counts = {}
# "Sort by top of stack" section: "<count> <symbol>" or "??? [0x...]"
in_top = False
for line in open(sample_path):
    if "Sort by top of stack" in line:
        in_top = True
        continue
    if in_top:
        if line.startswith("Binary Images") or not line.strip():
            if line.startswith("Binary Images"):
                break
            continue
        m = re.match(r"\s*(.+?)\s+(\d+)\s*$", line.rstrip())
        if not m:
            continue
        sym, n = m.group(1).strip(), int(m.group(2))
        hexm = re.search(r"\[0x([0-9a-f]+)\]", sym)
        if hexm:
            js = symbolize(int(hexm.group(1), 16))
            if js:
                sym = f"JIT {js}"
        counts[sym] = counts.get(sym, 0) + n

total = sum(counts.values()) or 1
print(f"{'samples':>8}  {'pct':>6}  symbol")
for sym, n in sorted(counts.items(), key=lambda kv: -kv[1])[:25]:
    if "__psynch_cvwait" in sym or "__ulock_wait" in sym:
        continue  # parked pool threads
    print(f"{n:>8}  {100.0*n/total:>5.1f}%  {sym}")
print(f"\n(jit regions: {len(regions)})")

# hot offsets within the top JIT region — feeds the disassembler
jit_off = {}
for sym, n in counts.items():
    m = re.match(r"JIT (\S+)\+0x([0-9a-f]+)", sym)
    if m:
        jit_off.setdefault(m.group(1), []).append((int(m.group(2), 16), n))
for name, offs in sorted(jit_off.items(), key=lambda kv: -sum(n for _, n in kv[1])):
    tot = sum(n for _, n in offs)
    print(f"\nhot offsets in {name} (total {tot}):")
    for off, n in sorted(offs, key=lambda x: -x[1])[:12]:
        print(f"  +{off:#06x}  {n}")
    break
PY
rm -rf "$TMP"
