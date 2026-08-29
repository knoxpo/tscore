// closures + cells
function counter() {
    let n = 0;
    return () => { n = n + 1; return n; };
}
const c = counter();
c(); c();
console.log("counter:", c());

// loops, arrays, objects
const arr: number[] = [];
for (let i = 0; i < 5; i++) arr.push(i * i);
console.log("squares:", arr, "len:", arr.length);
let sum = 0;
for (const x of arr) sum += x;
console.log("sum:", sum);
const obj = { name: "tscore", version: 0.1 };
obj.version = 0.2;
console.log(obj.name, obj["version"]);

// operators
console.log("bit:", (255 & 15) | 16, 1 << 10, -7 >>> 0);
console.log("cmp:", 3 > 2 && "a" < "b", typeof obj, typeof c);
console.log("tern:", sum > 10 ? "big" : "small");
console.log("math:", Math.floor(3.7), Math.sqrt(81), Math.imul(3, 4));
console.log("str:", "abc".length, "abc".charCodeAt(1));

// higher-order + parallel stub
const doubled = await parallel.map([1, 2, 3], (x: number) => x * 2);
console.log("parallel:", doubled);

// while/break/continue
let i = 0, hits = 0;
while (true) {
    i++;
    if (i % 2 === 0) continue;
    if (i > 9) break;
    hits++;
}
console.log("hits:", hits);
