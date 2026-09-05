// Fan-out cost: hand N trivial items to the pool and get results back.
// Measures scheduler + clone overhead with (almost) no compute.
const SIZES = [8, 64, 512, 4096];
function build(n: number): number[] {
    const a: number[] = [];
    for (let i = 0; i < n; i++) a.push(i);
    return a;
}
const bump = (x: number) => x + 1;
let checksum = 0;
await parallel.map(build(64), bump);                 // warm the pool
for (const n of SIZES) {
    const items = build(n);
    const t0 = performance.now();
    const out = await parallel.map(items, bump);
    const t1 = performance.now();
    let s = 0;
    for (const v of out) s += v;
    checksum += s;
    console.log(`FANOUT ${n} ${t1 - t0}`);
}
console.log(`RESULT ${checksum}`);
console.log(`WORKERS ${runtime.cpu.count}`);
