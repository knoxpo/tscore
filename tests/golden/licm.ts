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

// 6. The shadow-slot path: an invariant modulo used as a call argument,
// so the callee frame clobbers its home on every iteration. The array
// push is what lets a loop with a call reach the optimizing tier.
function addTwo(a, b) {
    return a + b;
}
function calls(outer, inner) {
    let acc = 0;
    for (let r = 0; r < outer; r++) {
        const got = [];
        for (let i = 0; i < inner; i++) got.push(addTwo(i, r % 7));
        for (let i = 0; i < got.length; i++) acc = acc + got[i];
    }
    return acc;
}

// 7. The hoisted modulo's dividend stops being an integer, so the
// preheader's magic-division path has to give way mid-run.
function shadowDrift(n) {
    let k = 9;
    let acc = 0;
    for (let r = 0; r < n; r++) {
        const got = [];
        for (let i = 0; i < 4; i++) got.push(addTwo(i, k % 7));
        for (let i = 0; i < got.length; i++) acc = acc + got[i];
        k = k + 1.25;
    }
    return acc;
}

// 8. Zero trips: nothing the preheader computes may escape the loop.
function zeroCalls(outer, inner) {
    let seen = -5;
    for (let r = 0; r < outer; r++) {
        const got = [];
        for (let i = 0; i < inner; i++) got.push(addTwo(i, r % 7));
        seen = seen + got.length;
    }
    return seen;
}

// 9. `if (c) x;` in the body compiles to the same `Skip; Jump` pair as
// the loop test. Its jump lands inside the loop, so the guarded arm is
// not on the spine and must not be hoisted — it ran from iteration 0.
function guarded(n) {
    let acc = 0;
    let f = 1;
    for (let i = 0; i < n; i++) {
        if (i === 3) f = 10;
        acc = acc + f;
    }
    return acc;
}

let out = 0;
for (let r = 0; r < 2000; r++) {
    out = out + zeroTrip(0);
    out = out + zeroTrip(3);
    out = out + nested(4, 5);
    out = out + drift(6);
    out = out + early(20);
    out = out + pushLoop(12);
    out = out + calls(9, 3);
    out = out + shadowDrift(5);
    out = out + zeroCalls(4, 0);
    out = out + zeroCalls(3, 2);
    out = out + guarded(8);
}
console.log(out);
console.log(zeroTrip(0), zeroTrip(1), nested(3, 3), drift(4), early(2), pushLoop(7));
console.log(calls(4, 2), shadowDrift(3), zeroCalls(3, 0), zeroCalls(2, 2));
console.log(guarded(0), guarded(3), guarded(4), guarded(8));
