// worker_threads port of primes.ts — the ergonomics baseline tscore competes with
import { Worker, isMainThread, parentPort, workerData } from "node:worker_threads";
import { fileURLToPath } from "node:url";
import os from "node:os";

function countPrimes(range) {
    let count = 0;
    for (let n = range.from; n < range.to; n++) {
        if (n < 2) continue;
        let isPrime = true;
        for (let d = 2; d * d <= n; d++) {
            if (n % d === 0) { isPrime = false; break; }
        }
        if (isPrime) count++;
    }
    return count;
}

if (isMainThread) {
    const W = Number(process.env.WORKERS || os.availableParallelism());
    const ranges = [];
    for (let i = 0; i < 400; i++) ranges.push({ from: i * 2000, to: (i + 1) * 2000 });
    const chunk = Math.ceil(ranges.length / W);
    const t0 = performance.now();
    const results = await Promise.all(
        Array.from({ length: W }, (_, w) => new Promise((resolve, reject) => {
            const slice = ranges.slice(w * chunk, (w + 1) * chunk);
            const worker = new Worker(fileURLToPath(import.meta.url), { workerData: slice });
            worker.once("message", resolve);
            worker.once("error", reject);
        }))
    );
    const total = results.flat().reduce((a, b) => a + b, 0);
    const t1 = performance.now();
    console.log(`RESULT ${total}`);
    console.log(`TIME_MS ${t1 - t0}`);
} else {
    parentPort.postMessage(workerData.map(countPrimes));
}
