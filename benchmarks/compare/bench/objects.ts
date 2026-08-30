// Monomorphic object create + property read/write hot loop.
// Shared source: runs unmodified on tscore, node, bun (subset-clean, no annotations).
function run(iters) {
    const pts = [];
    for (let i = 0; i < 1000; i++) pts.push({ x: i, y: i * 2, z: 0 });
    let acc = 0;
    for (let iter = 0; iter < iters; iter++) {
        for (let i = 0; i < pts.length; i++) {
            const p = pts[i];
            p.z = p.x + p.y + p.z % 1000;
            acc += p.z % 7;
        }
    }
    return acc % 1000000007;
}
run(500); // warmup (lets JIT tiers kick in; tscore just pays it)
const t0 = performance.now();
const r = run(20000);
const t1 = performance.now();
console.log(`RESULT ${r}`);
console.log(`TIME_MS ${t1 - t0}`);
