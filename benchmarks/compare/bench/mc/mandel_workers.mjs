// worker_threads port of benchmarks/programs/mandelbrot.ts (runs on node and bun).
// WORKERS env selects thread count.
import { Worker, isMainThread, parentPort, workerData } from "node:worker_threads";
import { fileURLToPath } from "node:url";
import os from "node:os";

function mandelRow(y) {
    const H = 240, W = 320, MAXI = 500;
    const ci = (y / H) * 2 - 1;
    let inside = 0;
    for (let px = 0; px < W; px++) {
        const cr = (px / W) * 3 - 2;
        let zr = 0, zi = 0, i = 0;
        while (i < MAXI && zr * zr + zi * zi <= 4) {
            const t = zr * zr - zi * zi + cr;
            zi = 2 * zr * zi + ci;
            zr = t;
            i++;
        }
        if (i === MAXI) inside++;
    }
    return inside;
}

if (isMainThread) {
    const W = Number(process.env.WORKERS || os.availableParallelism());
    const rows = [];
    for (let y = 0; y < 240; y++) rows.push(y);
    const chunk = Math.ceil(rows.length / W);
    const t0 = performance.now();
    const results = await Promise.all(
        Array.from({ length: W }, (_, w) => new Promise((resolve, reject) => {
            const slice = rows.slice(w * chunk, (w + 1) * chunk);
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
    parentPort.postMessage(workerData.map(mandelRow));
}
