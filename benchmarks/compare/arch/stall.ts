// Pause detector: build a 1M-object live set, then churn allocations for
// 5 seconds while timestamping every 1000 iterations.  The largest gap
// between timestamps is the longest the program was stopped for any
// reason (GC, JIT, OS).  Same file on every engine: no GC hooks needed.
const LIVE = 1000000;
const keep: { a: number, b: number[] }[] = [];
for (let i = 0; i < LIVE; i++) keep.push({ a: i, b: [i, i + 1] });

const BIN_US = 100;                       // histogram resolution 0.1 ms
const BINS = 20000;                       // up to 2 s
const hist: number[] = [];
for (let i = 0; i < BINS; i++) hist.push(0);

let maxGap = 0, samples = 0, iters = 0;
const start = performance.now();
let last = start;
let sink = 0;
while (performance.now() - start < 5000) {
    for (let k = 0; k < 1000; k++) {
        const o = { x: iters, y: [iters, iters + 1], z: `s${k}` };
        sink += o.y[1] - o.x;
        if (k === 999) keep[iters % LIVE] = { a: iters, b: o.y };   // mutate old gen: barrier + promotion
        iters++;
    }
    const now = performance.now();
    const gap = now - last;
    last = now;
    if (gap > maxGap) maxGap = gap;
    let bin = (gap * 1000 / BIN_US) | 0;
    if (bin >= BINS) bin = BINS - 1;
    hist[bin] = hist[bin] + 1;
    samples++;
}
let seen = 0, p99 = 0, p999 = 0;
for (let i = 0; i < BINS; i++) {
    seen += hist[i];
    if (p99 === 0 && seen >= samples * 0.99) p99 = (i + 1) * BIN_US / 1000;
    if (p999 === 0 && seen >= samples * 0.999) p999 = (i + 1) * BIN_US / 1000;
}
console.log(`MAX_STALL_MS ${maxGap}`);
console.log(`P99_MS ${p99}`);
console.log(`P999_MS ${p999}`);
console.log(`ITERS ${iters}`);
console.log(`RESULT ${sink > 0 ? 1 : 0}`);
