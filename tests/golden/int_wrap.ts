// Arithmetic whose result only bitwise ops read may wrap to i32 instead
// of checking for overflow. What must still hold when it does.

// 1. The idiom itself: a product and a sum well past i32, ToInt32'd.
function wrapMul(n) {
    let h = 1;
    for (let i = 0; i < n; i++) h = (h * 31 + i) | 0;
    return h;
}
function wrapAdd(n) {
    let acc = 0;
    for (let i = 0; i < n; i++) acc = (acc + i * 2 + 100000) | 0;
    return acc;
}

// 2. A wrapped value read by every bitwise op, not just `| 0`.
function wrapOps(n) {
    let a = 0, b = 0, c = 0, d = 0;
    for (let i = 0; i < n; i++) {
        a = (a + i * 99991) & 2147483647;
        b = (b + i * 99991) ^ 5;
        c = ((c + i * 99991) >> 3) | 0;
        d = ((d + i * 99991) >>> 1) | 0;
    }
    return a + b + c + d;
}

// 3. The same sum read by something that is NOT a ToInt32: it must stay
// an exact double, so the overflow check has to remain.
function noWrap(n) {
    let acc = 0;
    for (let i = 0; i < n; i++) acc = acc + i * 1000000;
    return acc;
}

// 4. A value both a bitwise op and a plain read consume — also no wrap.
function mixedReaders(n) {
    let acc = 0;
    let seen = 0;
    for (let i = 0; i < n; i++) {
        const t = acc + i * 1000000;
        seen = seen + (t % 7);
        acc = t | 0;
    }
    return acc + seen;
}

// 5. Negative zero: ToInt32(-0) is 0, but an unwrapped reader keeps it.
function negZero(n) {
    let z = 0;
    let s = "";
    for (let i = 0; i < n; i++) z = (0 * -1) | 0;
    s = s + (1 / (0 * -1));
    return s + " " + z + " " + (1 / z);
}

// 6. Wrapping under a loop the interpreter may resume mid-way: the home
// the deopt hands back holds the wrapped value, and the interpreter's
// own ToInt32 has to agree with it.
function drifts(n) {
    let acc = 0;
    let f = 1;
    for (let i = 0; i < n; i++) {
        acc = (acc + i * 77777) | 0;
        if (i === 900) f = 0.5;
        acc = (acc + f * 2) | 0;
    }
    return acc;
}

let out = 0;
for (let r = 0; r < 300; r++) {
    out = (out + wrapMul(1200)) | 0;
    out = (out + wrapAdd(1200)) | 0;
    out = (out + wrapOps(1200)) | 0;
    out = (out + drifts(1200)) | 0;
}
console.log(out);
console.log(wrapMul(1200), wrapAdd(1200), wrapOps(1200), drifts(1200));
console.log(wrapMul(0), wrapAdd(1), wrapOps(3), drifts(2));
console.log(noWrap(1200), mixedReaders(1200));
console.log(negZero(5));
