// Loop-invariant code motion: the cases the hoist rules turn on.

// 1. A zero-trip loop must not publish anything its body would compute.
function zeroTrip(n) {
    let t = -1;
    let i = 0;
    for (i = 0; i < n; i++) {
        t = 7;
        const k = n % 3;
        t = t + k;
    }
    return t;
}

// 2. An invariant modulo in the inner loop, read every iteration.
function nested(outer, inner) {
    let acc = 0;
    for (let a = 0; a < outer; a++) {
        for (let b = 0; b < inner; b++) {
            acc = acc + (a % 5) + (a % 5);
        }
    }
    return acc;
}

// 3. The hoisted value's operand stops being an integer: the modulo the
// preheader runs must fall back without losing the iteration.
function drift(n) {
    let x = 3;
    let acc = 0;
    for (let i = 0; i < n; i++) {
        for (let j = 0; j < 3; j++) acc = acc + (x % 4);
        x = x + 0.5;
    }
    return acc;
}

// 4. An early exit: a value the loop hoists must be dead on that edge.
function early(n) {
    let acc = 0;
    for (let i = 0; i < n; i++) {
        const step = n % 9;
        if (i > 4) return acc;
        acc = acc + step;
    }
    return acc;
}

// 5. Invariant Move of an array, pushed to every iteration.
function pushLoop(n) {
    const dst = [];
    for (let i = 0; i < n; i++) {
        const target = dst;
        target.push(i % 6);
    }
    let s = 0;
    for (let i = 0; i < dst.length; i++) s = s + dst[i];
    return s;
}

let out = 0;
for (let r = 0; r < 2000; r++) {
    out = out + zeroTrip(0);
    out = out + zeroTrip(3);
    out = out + nested(4, 5);
    out = out + drift(6);
    out = out + early(20);
    out = out + pushLoop(12);
}
console.log(out);
console.log(zeroTrip(0), zeroTrip(1), nested(3, 3), drift(4), early(2), pushLoop(7));
