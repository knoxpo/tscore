// node/bun counterpart of fanout.ts: a pool of worker_threads, one
// postMessage round-trip per item, pool created and warmed before timing.
import { Worker, isMainThread, parentPort } from "node:worker_threads";
import { fileURLToPath } from "node:url";
import os from "node:os";

const SIZES = [8, 64, 512, 4096];
if (isMainThread) {
    const W = Number(process.env.WORKERS || os.availableParallelism());
    const workers = [];
    const pending = [];
    for (let i = 0; i < W; i++) {
        const w = new Worker(fileURLToPath(import.meta.url));
        w.on("message", (m) => { const r = pending[i].shift(); r(m); });
        workers.push(w); pending.push([]);
    }
    const call = (x, i) => new Promise((resolve) => { pending[i % W].push(resolve); workers[i % W].postMessage(x); });
    const run = async (n) => {
        const ps = [];
        for (let i = 0; i < n; i++) ps.push(call(i, i));
        const out = await Promise.all(ps);
        let s = 0; for (const v of out) s += v;
        return s;
    };
    await run(64);                                       // warm
    let checksum = 0;
    for (const n of SIZES) {
        const t0 = performance.now();
        checksum += await run(n);
        const t1 = performance.now();
        console.log(`FANOUT ${n} ${t1 - t0}`);
    }
    console.log(`RESULT ${checksum}`);
    console.log(`WORKERS ${W}`);
    for (const w of workers) await w.terminate();
} else {
    parentPort.on("message", (x) => parentPort.postMessage(x + 1));
}
