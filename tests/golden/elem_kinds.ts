// Element kinds: a typed index site must survive every way an array can
// change what it holds.
function sum(a, n) { let s = 0; for (let k = 0; k < n; k++) for (let i = 0; i < a.length; i++) s = (s + a[i]) % 1000003; return s; }
const ints = []; for (let i = 0; i < 100; i++) ints.push(i);
console.log("ints:", sum(ints, 3000));
ints[50] = 1.5;                        // int array takes a double while the site is hot
console.log("after double:", sum(ints, 3000));
const grow = [];
for (let i = 0; i < 50; i++) { grow.push(i); if (i === 25) grow.push(0.5); }
console.log("grow:", grow.length, grow[26], sum(grow, 5));
const lit = [1, 2, 3];
lit[0] = 9.5;
console.log("lit:", lit[0], lit[1], sum(lit, 5));
function pick(a, i) { return a[i]; }
const mixed = [1, 2.5, 3];
console.log("mixed:", pick(mixed, 0), pick(mixed, 1), pick(mixed, 2));
const doubles = []; for (let i = 0; i < 60; i++) doubles.push(i + 0.25);
console.log("doubles:", sum(doubles, 2000));
