// node/bun counterpart of channel_tscore.ts: worker thread consumes 100k messages
// posted from main, replies with the sum. worker_threads runs on both node and bun.
import { Worker, isMainThread, parentPort } from "node:worker_threads";
import { fileURLToPath } from "node:url";

const N = 100000;
if (isMainThread) {
    const worker = new Worker(fileURLToPath(import.meta.url));
    const t0 = performance.now();
    const done = new Promise((resolve) => worker.once("message", resolve));
    for (let i = 0; i < N; i++) worker.postMessage(i);
    worker.postMessage(null);
    const sum = await done;
    const t1 = performance.now();
    console.log(`RESULT ${sum}`);
    console.log(`TIME_MS ${t1 - t0}`);
    await worker.terminate();
} else {
    let sum = 0;
    parentPort.on("message", (v) => {
        if (v === null) {
            parentPort.postMessage(sum);
        } else {
            sum += v;
        }
    });
}
