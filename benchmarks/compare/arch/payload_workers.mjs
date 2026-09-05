// node/bun counterpart of payload.ts: one worker echoes the payload via postMessage.
import { Worker, isMainThread, parentPort } from "node:worker_threads";
import { fileURLToPath } from "node:url";

const SIZES = [10, 1000, 100000];
const ROUNDS = 20;
function build(n) {
    const a = [];
    for (let i = 0; i < n; i++) a.push({ id: i, name: `item-${i}`, tags: [i, i * 2, i * 3] });
    return a;
}
if (isMainThread) {
    const worker = new Worker(fileURLToPath(import.meta.url));
    let resolve;
    worker.on("message", (m) => resolve(m));
    const call = (p) => new Promise((r) => { resolve = r; worker.postMessage(p); });
    await call(build(10));                              // warm
    let checksum = 0;
    for (const n of SIZES) {
        const payload = build(n);
        const t0 = performance.now();
        let got = 0;
        for (let r = 0; r < ROUNDS; r++) got += (await call(payload)).length;
        const t1 = performance.now();
        checksum += got;
        console.log(`PAYLOAD ${n} ${(t1 - t0) / ROUNDS}`);
    }
    console.log(`RESULT ${checksum}`);
    await worker.terminate();
} else {
    parentPort.on("message", (p) => parentPort.postMessage(p));
}
