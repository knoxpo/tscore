// arrays that cross a realm boundary and pass through natives
const rows = []; for (let i = 0; i < 40; i++) rows.push(i);
const out = await parallel.map(rows, (n: number): number => { const local = [n, n * 2, n * 3]; let s = 0; for (let i = 0; i < local.length; i++) s += local[i]; return s; });
let total = 0; for (const v of out) total += v;
console.log("parallel:", total);
const bytes = runtime.bytes;
const b = bytes.fromArray([1, 2, 3, 250]);
console.log("bytes:", bytes.size(b), bytes.toArray(b)[3]);
const widened = [1, 2, 3]; widened[1] = 7.5;
console.log("widened via native:", bytes.size(bytes.fromArray([1, 2, 3])), widened[1]);
