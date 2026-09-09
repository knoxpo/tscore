// Pretenured mixed churn across several major collections. Closures,
// objects and array literals all age through `bulk_promote`, which must
// clear the mark bit it inherits: a stale mark makes the next major
// treat a cell as already traced and sweep everything reachable only
// through it. Needs TSC_PRETENURE=1 to bite, which the differential
// gate runs; several rounds because one major introduces the damage and
// a later one trips over it.
function makeNode(i) { return { id: i, data: [i, i * 2, i * 3, i * 4], next: null }; }
function mk(a, b) { const bias = a * 3 + b; return (x) => x + bias % 100; }
function run(total) {
    const K = 20000;
    const ring = [];
    const fns = [];
    for (let i = 0; i < K; i++) ring.push(makeNode(i));
    let acc = 0;
    for (let i = 0; i < total; i++) {
        const n = makeNode(i);
        n.next = ring[(i + 1) % K];
        ring[i % K] = n;
        if (fns.length < 4000) fns.push(mk(i, i % 50)); else fns[i % 4000] = mk(i, i % 50);
        acc = (acc + n.data[2] + fns[i % 4000](i % 13)) % 1000000007;
    }
    return acc;
}
let t = 0;
for (let r = 0; r < 3; r++) t = (t + run(400000)) % 1000000007;
console.log(t);
