const counter = actor(() => {
    let n = 0;
    return {
        add: (msg: { type: string, by: number }) => { n = n + msg.by; },
        get: () => n,
    };
});
const flaky = actor(() => {
    let ok = 0;
    return {
        work: () => { ok = ok + 1; },
        boom: () => missingGlobal,
        count: () => ok,
    };
});
for (let i = 0; i < 50; i++) {
    counter.post({ type: "add", by: 2 });
    flaky.post({ type: "work" });
}
flaky.post({ type: "boom" });        // fails, logged, actor survives
flaky.post({ type: "work" });
console.log("counter:", await counter.send({ type: "get" }));
console.log("flaky survived:", await flaky.send({ type: "count" }));
console.log("counter unaffected:", await counter.send({ type: "get" }));
counter.stop();
flaky.stop();
