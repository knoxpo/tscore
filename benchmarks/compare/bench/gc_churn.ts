// GC pressure: rotating live set. Ring of K live object graphs, constantly replaced,
// so collections always have real live data to mark (tscore threshold: 256k allocs).
function makeNode(i) {
    return { id: i, data: [i, i * 2, i * 3, i * 4], next: null };
}
function run(total) {
    const K = 20000;
    const ring = [];
    for (let i = 0; i < K; i++) ring.push(makeNode(i));
    let acc = 0;
    for (let i = 0; i < total; i++) {
        const n = makeNode(i);
        n.next = ring[(i + 1) % K];
        ring[i % K] = n;
        acc = (acc + n.data[2]) % 1000000007;
    }
    return acc;
}
run(200000); // warmup
const t0 = performance.now();
const r = run(3000000);
const t1 = performance.now();
console.log(`RESULT ${r}`);
console.log(`TIME_MS ${t1 - t0}`);
