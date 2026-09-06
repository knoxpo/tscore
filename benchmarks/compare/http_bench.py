"""HTTP hello-world benchmark: tscore vs node vs bun.

Reports req/s AND the CPU actually consumed, because req/s alone is
meaningless here: this machine's loopback path caps total HTTP
throughput at roughly 130-155k req/s no matter who serves it. Proof:
four INDEPENDENT server processes, each with its own wrk, total the
same as one server alone. So a multi-worker configuration can look
"faster" while only burning more cores for a few percent.

req/s per core is the number that actually compares engines here.
Every engine is measured in both its single and multi configurations,
including bun's reusePort (Bun.serve is single-threaded otherwise).

With --repeat N the whole sweep runs N times and each engine is reported
at its best, plus a spread table. Best rather than median because
contention only ever costs throughput: a busy moment can only drag a
number down, so the maximum is the one converging on what the engine can
do, while a median tracks how busy the box happened to be. The spread is
the honest part — it says how much the box moved underneath the table.

Usage: python3 benchmarks/compare/http_bench.py [workers] [--repeat N]
"""
import subprocess, time, re, os, sys, argparse

ap = argparse.ArgumentParser()
ap.add_argument("workers", nargs="?", type=int, default=8)
ap.add_argument("--repeat", type=int, default=1,
                help="run the whole sweep N times; report each engine's best")
args = ap.parse_args()
W = args.workers
REPEAT = max(1, args.repeat)

S = os.path.dirname(os.path.abspath(__file__)) + "/generated"
os.makedirs(S, exist_ok=True)
BIN = os.environ.get("TSCORE", "./target/release/tscore")

def cputime(pids):
    """Exact accumulated CPU seconds across a process tree."""
    tot = 0.0
    for pid in pids:
        o = subprocess.run(["ps","-o","cputime=","-p",str(pid)],
                           capture_output=True,text=True).stdout.strip()
        if not o: continue
        # formats: MM:SS.ss or HH:MM:SS
        parts = o.split(":")
        try:
            if len(parts) == 2: tot += int(parts[0])*60 + float(parts[1])
            elif len(parts) == 3: tot += int(parts[0])*3600 + int(parts[1])*60 + float(parts[2])
        except ValueError: pass
    return tot

def tree(root):
    pids = [root]
    out = subprocess.run(["pgrep","-P",str(root)],capture_output=True,text=True).stdout.split()
    for k in out: pids.extend(tree(int(k)))
    return pids

def measure(name, port, pids, dur=5):
    pids = sum([tree(p) for p in pids], [])
    c0 = cputime(pids); t0 = time.time()
    o = subprocess.run(["wrk","-t2","-c32",f"-d{dur}s",f"http://127.0.0.1:{port}/"],
                       capture_output=True,text=True).stdout
    wall = time.time() - t0
    c1 = cputime(pids)
    m = re.search(r"Requests/sec:\s+([\d.]+)", o)
    rps = float(m.group(1)) if m else 0.0
    cores = (c1 - c0) / wall
    per_core = rps / cores if cores > 0.05 else float("nan")
    sys.stdout.flush()
    return rps, cores, per_core

# bun reusePort xW. On darwin SO_REUSEPORT keeps BSD semantics: it lets
# the processes share the port but does NOT load-balance across them, so
# the last binder serves everything and the other W-1 sit idle (verified
# directly: 200 requests, one pid). The row is labelled for what it
# actually measures there — the 1.0 cores column is the tell.
BUN_MULTI = (f"bun x{W} reusePort"
             + (" (darwin: 1 active, %d idle)" % (W - 1) if sys.platform == "darwin" else ""))

def sweep():
    """One full pass over every engine. Returns {name: (rps, cores, per_core)}."""
    rows = {}
    # tscore W workers
    open(f"{S}/g_ts.ts","w").write(f'await runtime.http.serve(41970, () => "hello world", {{ workers: {W} }});\n')
    p = subprocess.Popen([BIN,"run",f"{S}/g_ts.ts","--no-stats-export"],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    time.sleep(1.5); rows[f"tscore workers:{W}"] = measure(f"tscore workers:{W}", 41970, [p.pid]); p.kill(); time.sleep(1)
    # tscore 1 worker
    open(f"{S}/g_ts1.ts","w").write('await runtime.http.serve(41971, () => "hello world", { workers: 1 });\n')
    p = subprocess.Popen([BIN,"run",f"{S}/g_ts1.ts","--no-stats-export"],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    time.sleep(1.5); rows["tscore workers:1"] = measure("tscore workers:1", 41971, [p.pid]); p.kill(); time.sleep(1)
    # node cluster
    open(f"{S}/g_nd.mjs","w").write(f'''
import {{ createServer }} from "node:http";
import cluster from "node:cluster";
if (cluster.isPrimary) {{ for (let i = 0; i < {W}; i++) cluster.fork(); }}
else createServer((req, res) => res.end("hello world")).listen(41972, "127.0.0.1");
''')
    p = subprocess.Popen(["node",f"{S}/g_nd.mjs"],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    time.sleep(2.0); rows[f"node cluster x{W}"] = measure(f"node cluster x{W}", 41972, [p.pid])
    subprocess.run(["pkill","-f","g_nd.mjs"]); time.sleep(1)
    # node single
    open(f"{S}/g_nd1.mjs","w").write('import { createServer } from "node:http";\ncreateServer((q,r)=>r.end("hello world")).listen(41973,"127.0.0.1");\n')
    p = subprocess.Popen(["node",f"{S}/g_nd1.mjs"],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    time.sleep(1.5); rows["node x1"] = measure("node x1", 41973, [p.pid]); p.kill(); time.sleep(1)
    # bun reusePort xW
    open(f"{S}/g_bun.ts","w").write('Bun.serve({ port: 41974, hostname: "127.0.0.1", reusePort: true, fetch() { return new Response("hello world"); } });\n')
    bps=[subprocess.Popen(["bun",f"{S}/g_bun.ts"],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL) for _ in range(W)]
    time.sleep(2.0); rows[BUN_MULTI] = measure(BUN_MULTI, 41974, [b.pid for b in bps])
    for b in bps: b.kill()
    time.sleep(1)
    # bun single
    open(f"{S}/g_bun1.ts","w").write('Bun.serve({ port: 41975, hostname: "127.0.0.1", fetch() { return new Response("hello world"); } });\n')
    b=subprocess.Popen(["bun",f"{S}/g_bun1.ts"],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    time.sleep(1.5); rows["bun x1"] = measure("bun x1", 41975, [b.pid]); b.kill()
    time.sleep(1)
    return rows

best = {}          # name -> (rps, cores, per_core) of that engine's best run
seen = {}          # name -> [rps, ...] across every run
for i in range(REPEAT):
    if REPEAT > 1:
        print(f"  sweep {i+1}/{REPEAT}", file=sys.stderr, flush=True)
    for name, r in sweep().items():
        seen.setdefault(name, []).append(r[0])
        if name not in best or r[0] > best[name][0]:
            best[name] = r

title = "## HTTP hello (wrk -t2 -c32 -d5s"
title += f", best of {REPEAT})" if REPEAT > 1 else ")"
print(f"\n{title}\n")
print("| engine | req/s | cores | req/s per core |")
print("|---|---|---|---|")
top = max(v[2] for v in best.values())
for name, (rps, cores, pc) in sorted(best.items(), key=lambda kv: -kv[1][2]):
    mark = " **best/core**" if pc == top else ""
    print(f"| {name} | {rps:,.0f} | {cores:.1f} | {pc:,.0f}{mark} |")
print()
print("Total throughput is capped by this machine's loopback stack, not")
print("by any engine: four independent servers with four independent")
print("clients total the same as one. Compare the per-core column.")

if REPEAT > 1:
    print()
    print(f"Each engine at its best of {REPEAT} sweeps. Contention only ever")
    print("costs throughput, so the maximum is the estimate that converges on")
    print("the engine while a median would track how busy the box was. The")
    print("spread says how much it moved underneath the table:")
    print()
    print("| engine | min | max |")
    print("|---|---|---|")
    for name in sorted(seen, key=lambda n: -best[n][2]):
        v = seen[name]
        print(f"| {name} | {min(v):,.0f} | {max(v):,.0f} |")
    print()
    print("If an engine changes places between runs, the per-core ordering is")
    print("an artefact of when the sweep landed and the numbers are not usable.")

if sys.platform == "darwin":
    print()
    print(f"bun has no working multi-core HTTP on darwin: SO_REUSEPORT there")
    print(f"permits the shared bind but does not distribute, so one of the {W}")
    print("processes serves every connection and the rest idle. Its row is a")
    print("second bun x1 measurement, not a multi-core one. node's cluster")
    print("distributes in userspace via the primary, and tscore's workers")
    print("share one listener with a kqueue per thread; both scale here.")
