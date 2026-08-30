// Cross-thread messaging: producer -> consumer, 100k numbers through a bounded channel.
const N = 100000;
const ch = Channel.create({ capacity: 1024 });
async function producer() {
    for (let i = 0; i < N; i++) await Channel.send(ch, i);
    Channel.close(ch);
    return N;
}
async function consumer() {
    let sum = 0;
    while (true) {
        const v = await Channel.recv(ch);
        if (v === undefined) break;
        sum += v;
    }
    return sum;
}
const t0 = performance.now();
const p = producer();
const c = consumer();
await p;
const sum = await c;
const t1 = performance.now();
console.log(`RESULT ${sum}`);
console.log(`TIME_MS ${t1 - t0}`);
