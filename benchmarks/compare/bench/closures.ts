// Closure allocation + invocation: create closures capturing locals, call them all.
function makeAdder(a, b) {
    const bias = a * 3 + b;
    return (x) => x + bias % 100;
}
function run(iters) {
    let acc = 0;
    for (let iter = 0; iter < iters; iter++) {
        const fns = [];
        for (let i = 0; i < 2000; i++) fns.push(makeAdder(i, iter % 50));
        for (let i = 0; i < fns.length; i++) acc += fns[i](i % 13);
        acc = acc % 1000000007;
    }
    return acc;
}
run(20); // warmup
const t0 = performance.now();
const r = run(3000);
const t1 = performance.now();
console.log(`RESULT ${r}`);
console.log(`TIME_MS ${t1 - t0}`);
