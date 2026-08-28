// Two independent actors + failure isolation.
const counter = actor(() => {
    let n = 0;
    return {
        add: (msg: { type: string, by: number }) => { n = n + msg.by; },
        get: () => n,
    };
});

const store = actor(() => {
    const items: string[] = [];
    return {
        put: (msg: { type: string, value: string }) => { items.push(msg.value); },
        size: () => items.length,
        boom: () => missingGlobal,   // deliberate failure
    };
});

// fire-and-forget in parallel across both actors
for (let i = 0; i < 100; i++) {
    counter.post({ type: "add", by: i });
    store.post({ type: "put", value: `item-${i}` });
}

console.log("counter:", counter.send({ type: "get" }));
console.log("store:", store.send({ type: "size" }));
console.log("counter again:", counter.send({ type: "get" }));
counter.stop();
store.stop();
