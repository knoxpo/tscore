// Promise machinery: deep await chains + wide start-then-await fanout.
// await-only (tscore has no .then) — same shape runs on all engines.
async function chain(n) {
    if (n === 0) return 0;
    return (await chain(n - 1)) + 1;
}
async function leaf(x) {
    return x * 2 + 1;
}
async function run(iters) {
    let acc = 0;
    for (let iter = 0; iter < iters; iter++) {
        acc += await chain(200);
        const ps = [];
        for (let i = 0; i < 200; i++) ps.push(leaf(i));
        for (let i = 0; i < ps.length; i++) acc += await ps[i];
        acc = acc % 1000000007;
    }
    return acc;
}
await run(50); // warmup
const t0 = performance.now();
const r = await run(5000);
const t1 = performance.now();
console.log(`RESULT ${r}`);
console.log(`TIME_MS ${t1 - t0}`);
