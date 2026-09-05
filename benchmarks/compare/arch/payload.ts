// Boundary-crossing cost vs payload size: send a nested object to another
// realm and get it echoed back, 20 round trips per size.
const SIZES = [10, 1000, 100000];        // ~1 KB, ~100 KB, ~10 MB
const ROUNDS = 20;
function build(n: number): { id: number, name: string, tags: number[] }[] {
    const a: { id: number, name: string, tags: number[] }[] = [];
    for (let i = 0; i < n; i++) a.push({ id: i, name: `item-${i}`, tags: [i, i * 2, i * 3] });
    return a;
}
const echo = (o: { id: number, name: string, tags: number[] }[]) => o;
let checksum = 0;
await parallel.map([build(10)], echo);                 // warm
for (const n of SIZES) {
    const payload = build(n);
    const t0 = performance.now();
    let got = 0;
    for (let r = 0; r < ROUNDS; r++) {
        const back = await parallel.map([payload], echo);
        got += back[0].length;
    }
    const t1 = performance.now();
    checksum += got;
    console.log(`PAYLOAD ${n} ${(t1 - t0) / ROUNDS}`);
}
console.log(`RESULT ${checksum}`);
