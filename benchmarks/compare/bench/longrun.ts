// Long-running stability: fixed 30s of mixed compute + allocation.
// Reports throughput (OPS = units/sec) and STABILITY (last-decile / first-decile
// throughput; < 1.0 means the engine slowed down as heap/JIT state aged).
// No cross-engine RESULT check — iteration counts are time-dependent.
function unit(seed) {
    const tmp = [];
    for (let i = 0; i < 50; i++) tmp.push({ v: (seed + i) * 3 % 997, w: [i, seed] });
    let s = 0;
    for (let i = 0; i < tmp.length; i++) s += tmp[i].v + tmp[i].w[0];
    for (let i = 0; i < 200; i++) s = (s * 31 + i) % 1000003;
    return s;
}
const DURATION_MS = 30000;
const deciles = [];
const t0 = Date.now();
let ops = 0;
let acc = 0;
let bucket = 0;
let bucketOps = 0;
while (true) {
    acc = (acc + unit(ops)) % 1000000007;
    ops++;
    bucketOps++;
    const el = Date.now() - t0;
    if (el >= DURATION_MS) break;
    const b = Math.floor(el / (DURATION_MS / 10));
    if (b !== bucket) {
        deciles.push(bucketOps);
        bucket = b;
        bucketOps = 0;
    }
}
deciles.push(bucketOps);
const elapsed = Date.now() - t0;
const first = deciles[0];
const last = deciles[deciles.length - 1];
console.log(`CHECK ${acc}`);
console.log(`OPS ${Math.floor(ops * 1000 / elapsed)}`);
console.log(`STABILITY ${Math.floor(last * 1000 / first) / 1000}`);
