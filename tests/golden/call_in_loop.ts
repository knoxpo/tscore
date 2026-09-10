// Calls inside a loop now reach the optimizing tier. The shapes below are
// the ones that broke while they did not.

// 1. A bitwise result used as a call argument. The bitwise arm keeps its
// destination in the intermediate register and defers the home write; the
// call spills, and reads the home. Dropping that deferred write left the
// slot holding whatever it did before — here the `Math` object itself.
function hash(seed) {
    let x = seed | 0;
    let h = 2166136261 | 0;
    for (let i = 0; i < 400; i++) {
        x = (x ^ (x << 13)) | 0;
        x = (x ^ (x >>> 17)) | 0;
        h = Math.imul(h ^ (x & 255), 16777619);
    }
    return h >>> 0;
}

// 2. The same through a hoisted binding, so the callee slot is not
// rewritten by a method lookup on every iteration.
function hashBound(seed) {
    const im = Math.imul;
    let x = seed | 0;
    let h = 1 | 0;
    for (let i = 0; i < 400; i++) {
        x = (x ^ (x << 7)) | 0;
        h = im(h ^ 1, 3);
    }
    return h | 0;
}

// 3. A user function called with a bitwise argument, and the counter
// laned alongside it.
function pick(a, b) {
    return a - b;
}
function mix(n) {
    let acc = 0;
    let x = 5;
    for (let i = 0; i < n; i++) {
        x = (x * 31 + i) | 0;
        acc = acc + pick(x & 63, i & 7);
    }
    return acc;
}

// 4. Natives in a loop: the arm that used to keep the whole function in
// the interpreter.
function trig(n) {
    let s = 0;
    for (let i = 1; i < n; i++) {
        s = s + Math.floor(i * 1.5) + Math.abs(i - 500) + Math.min(i, 7);
    }
    return s;
}

let out = 0;
for (let r = 0; r < 500; r++) {
    out = (out + hash(r)) % 1000000007;
    out = (out + hashBound(r)) % 1000000007;
    out = (out + mix(40)) % 1000000007;
    out = (out + trig(50)) % 1000000007;
}
console.log(out);
console.log(hash(7), hashBound(7), mix(40), trig(50));
console.log(hash(0), hashBound(0), mix(0), trig(1));
