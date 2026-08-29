// join results, unjoined children still awaited, nested scopes
function heavy(n: number): number {
    let s = 0;
    for (let i = 0; i < n; i++) s = (s + i * i) % 1000003;
    return s;
}
let sideEffectDone = false;
const total = task.scope((scope: any) => {
    const a = scope.spawn(() => heavy(200000));
    const b = scope.spawn(() => heavy(300000));
    const nested = scope.spawn(() => {
        return task.scope((inner: any) => {
            const x = inner.spawn(() => heavy(50000));
            const y = inner.spawn(() => heavy(60000));
            return x.join() + y.join();
        });
    });
    return a.join() + b.join() + nested.join();
});
console.log("total:", total);

// cancellation: cancelled child is discarded, siblings unaffected
const r = task.scope((scope: any) => {
    const doomed = scope.spawn(() => {
        let n = 0;
        while (true) { n++; }   // spins until cancelled at a back-edge
    });
    const fine = scope.spawn(() => heavy(100000));
    doomed.cancel();
    return fine.join();
});
console.log("after cancel:", r);
