// Integer-lane-eligible loops that suspend: an await resumes mid-loop,
// so the loop must not hold its counter or accumulator in a register.
async function leaf(x) { return x * 2 + 1; }
async function run(iters) {
    let acc = 0;
    for (let iter = 0; iter < iters; iter++) {
        const ps = [];
        for (let i = 0; i < 50; i++) ps.push(leaf(i));
        for (let i = 0; i < ps.length; i++) acc += await ps[i];
        acc = acc % 1000003;
        let s = 0;
        for (let k = 0; k < 20; k++) { s = (s * 31 + k) % 977; if (k % 7 === 3) s += await leaf(s); }
        acc = (acc + s) % 1000003;
    }
    return acc;
}
console.log(await run(300));
