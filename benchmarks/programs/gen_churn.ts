// Generational GC proof: a large LIVE old data set + heavy young churn.
// Full-GC runtimes re-mark the old array on every collection (O(heap));
// generational minors trace only the nursery (O(young)).
const OLD_SIZE = 1000000;
const old: { id: number, tag: string }[] = [];
for (let i = 0; i < OLD_SIZE; i++) old.push({ id: i, tag: `o${i % 100}` });

const t0 = performance.now();
let acc = 0;
const survivors: { id: number }[] = [];
for (let i = 0; i < 3000000; i++) {
    const tmp = { id: i, buf: [i, i + 1] };   // young garbage
    acc = (acc + tmp.buf[0] + old[i % OLD_SIZE].id) % 1000003;
    if (i % 100000 === 0) survivors.push({ id: i });
}
const t1 = performance.now();
console.log(`RESULT ${acc} ${survivors.length}`);
console.log(`TIME_MS ${t1 - t0}`);
