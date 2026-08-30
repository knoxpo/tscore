// Short-lived allocation churn: arrays + objects, nothing retained past the iteration.
function run(iters) {
    let acc = 0;
    for (let iter = 0; iter < iters; iter++) {
        const tmp = [];
        for (let i = 0; i < 100; i++) {
            tmp.push({ a: i, b: [i, i + 1, i + 2], c: `s${i % 10}` });
        }
        for (let i = 0; i < tmp.length; i++) acc += tmp[i].b[1];
        acc = acc % 1000000007;
    }
    return acc;
}
run(5000); // warmup
const t0 = performance.now();
const r = run(50000);
const t1 = performance.now();
console.log(`RESULT ${r}`);
console.log(`TIME_MS ${t1 - t0}`);
