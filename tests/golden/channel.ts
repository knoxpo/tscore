// backpressure: capacity 2, producer must wait for consumer
const ch = Channel.create({ capacity: 2 });
const log = Channel.create({ capacity: 100 });

async function producer(): Promise<number> {
    for (let i = 1; i <= 5; i++) {
        await Channel.send(ch, i);
        await Channel.send(log, `sent ${i}`);
    }
    Channel.close(ch);
    return 5;
}

async function consumer(): Promise<number> {
    let sum = 0;
    while (true) {
        await sleep(10); // slow consumer forces backpressure
        const v = await Channel.recv(ch);
        if (v === undefined) break;
        sum += v;
        await Channel.send(log, `got ${v}`);
    }
    return sum;
}

const p = producer();
const c = consumer();
console.log("produced:", await p, "consumed:", await c);

// channel crossing into parallel workers: distributed sum
const out = Channel.create({ capacity: 100 });
await parallel.for([1, 2, 3, 4, 5, 6, 7, 8], async (x: number) => {
    await Channel.send(out, x * x);
});
Channel.close(out);
let total = 0;
while (true) {
    const v = await Channel.recv(out);
    if (v === undefined) break;
    total += v;
}
console.log("squares total:", total);
