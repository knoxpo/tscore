const serialMap = (xs, f) => xs.map((x) => f(x));
function countPrimes(range) {
    let count = 0;
    for (let n = range.from; n < range.to; n++) {
        if (n < 2) continue;
        let isPrime = true;
        for (let d = 2; d * d <= n; d++) {
            if (n % d === 0) { isPrime = false; break; }
        }
        if (isPrime) count++;
    }
    return count;
}
const ranges = [];
for (let i = 0; i < 400; i++) ranges.push({ from: i * 2000, to: (i + 1) * 2000 });
const t0 = performance.now();
const counts = serialMap(ranges, countPrimes);
let total = 0;
for (const c of counts) total += c;
const t1 = performance.now();
console.log(`RESULT ${total}`);
console.log(`TIME_MS ${t1 - t0}`);
