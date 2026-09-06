// An accumulator the integer lane takes for an i32 outgrows it mid-loop.
// The header guard deopts; after three deopts the function recompiles
// without the lane instead of running interpreted for the rest of the
// program. Both paths must agree with double arithmetic.
function run(iters) {
    const xs = [];
    for (let i = 0; i < 1000; i++) xs.push(i);
    let acc = 0;
    for (let iter = 0; iter < iters; iter++) {
        for (let i = 0; i < xs.length; i++) {
            const v = xs[i];
            acc += (i % 1000) + (i % 7);
        }
    }
    return acc;
}
const a = run(8000);
console.log(a, a % 1000000007, a > 2147483647);
console.log(run(3));
