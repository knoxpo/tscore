// Field representation: shapes learn Int32/Number per field and widen in
// place when a store disagrees. Compiled code that typed a read must see
// the widening (shape id changes) and re-run correctly.
function mk(i) { return { a: i, b: i * 2, c: 0 }; }
function sum(pts, n) {
    let acc = 0;
    for (let r = 0; r < n; r++) {
        for (let i = 0; i < pts.length; i++) {
            const p = pts[i];
            p.c = (p.a + p.b + p.c) % 1000;
            acc += p.c % 7;
        }
    }
    return acc;
}
const pts = [];
for (let i = 0; i < 64; i++) pts.push(mk(i));
let total = sum(pts, 200);            // all Int32
pts[10].c = 0.5;                       // widen c to Number mid-life
total += sum(pts, 200);
pts[20].a = -0;                        // -0 is not Int32
total += sum(pts, 50);
pts[30].b = 1e300;                     // out of i32 range
total += sum(pts, 50);
let s = 0;
for (let i = 0; i < pts.length; i++) s += 1 / (pts[i].a === 0 ? pts[i].a : 1); // -0 sign survives
pts[40].c = "str";                     // widen to Any: arithmetic goes generic
let t = 0;
for (let r = 0; r < 100; r++) for (let i = 0; i < pts.length; i++) t += typeof pts[i].c === "string" ? 1 : 0;
// objects built by successive stores, then a transition
const o = {};
o.x = 1; o.y = 2;
for (let i = 0; i < 3000; i++) o.x = (o.x + o.y) % 97;
o.y = 2.5;
for (let i = 0; i < 3000; i++) o.x = (o.x + o.y) % 97;
o.z = 3;
console.log(total, s, t, o.x, o.y, o.z);
