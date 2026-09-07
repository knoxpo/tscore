// Integer representation edges: results must not depend on whether a
// number is carried as a tagged int or a double.
// -0 and tiny reciprocals are reported as words: console.log formatting
// of those is not what this golden tests
function show(label, v) { console.log(label, v === 0 ? (1 / v < 0 ? "-0" : "+0") : v, v < 0 ? "neg" : "pos", typeof v); }
let big = 2147483647;
show("add-overflow", big + 1);
show("sub-overflow", -2147483648 - 1);
show("mul-overflow", 65536 * 65536);
show("mul-neg-zero", -3 * 0);
show("mul-zero", 3 * 0);
show("mod-neg-zero", -6 % 3);
show("mod-sign", -7 % 3);
show("mod-zero", 5 % 0);
show("mod-min", -2147483648 % -1);
show("neg-zero", -0);
let z = 0;
show("neg-var-zero", -z);
show("neg-min", -(-2147483648));
show("div", 7 / 2);
show("div-exact", 8 / 2);
show("shl", 1 << 31);
show("ushr", -1 >>> 0);
show("ushr-small", 8 >>> 1);
show("bitand-double", 5.7 & 3);
show("bitnot", ~5);
show("bitor-big", 4294967296 | 0);
console.log(3 === 3.0, 0 === -0, 1 / 0 === Infinity, 2 + 0.5, 2 * 0.5, 1e10 + 1);
const arr = [10, 20, 30];
console.log(arr[1], arr[1.0], arr[-0], arr.length + 1, arr.length * 2.5, arr[3 - 2]);
console.log("s" + 42, "s" + (40 + 2), "s" + -0, "s" + 2147483648, "s" + 1.5 * 2);
let acc = 0;
for (let i = 0; i < 100; i++) acc = (acc * 31 + i) % 1000003;
console.log(acc, acc % 7, (acc | 0) >>> 3, acc / 3);
let f = 0.5;
for (let i = 0; i < 10; i++) f = f * 2 - 1;
console.log(f, f === -1, -1 === f);
console.log(2147483647 + 2147483647 - 2147483647, (2147483647 + 1) === 2147483648);
