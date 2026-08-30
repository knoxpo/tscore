// Timer/event-loop overhead proxy: N concurrent 1ms sleeps, awaited in batches.
// ponytail: sleep-storm stands in for async I/O — runtime has no sockets/fs yet (M4+).
// ponytail: batches of 200 — tscore spawns one OS thread per sleep() today.
async function batch(n) {
    const ps = [];
    for (let i = 0; i < n; i++) ps.push(sleep(1));
    for (let i = 0; i < ps.length; i++) await ps[i];
}
const t0 = performance.now();
for (let b = 0; b < 10; b++) await batch(200);
const t1 = performance.now();
console.log(`RESULT 2000`);
console.log(`TIME_MS ${t1 - t0}`);
