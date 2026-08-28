// messages are cloned: handler mutation cannot leak back to the caller
const mutator = actor(() => ({
    mutate: (msg: { type: string, data: number[] }) => {
        msg.data.push(999);
        return msg.data.length;
    },
}));
const payload = { type: "mutate", data: [1, 2, 3] };
console.log("handler saw:", mutator.send(payload));
console.log("caller kept:", payload.data.length);
mutator.stop();

// actors can spawn actors; state survives GC pressure
const outer = actor(() => {
    const inner = actor(() => {
        let total = 0;
        return {
            bump: (m: { type: string, by: number }) => { total = total + m.by; },
            total: () => total,
        };
    });
    return {
        churn: (m: { type: string, n: number }) => {
            for (let i = 0; i < m.n; i++) {
                const garbage = { tag: `g${i}`, arr: [i, i, i] };
                inner.post({ type: "bump", by: garbage.arr.length > 0 ? 1 : 0 });
            }
            return inner.send({ type: "total" });
        },
    };
});
console.log("nested total:", outer.send({ type: "churn", n: 100000 }));
outer.stop();
