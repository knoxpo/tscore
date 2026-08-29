// Count primes in [2, N) by trial division — deliberately skewed per-item cost.
function countPrimes(range: { from: number, to: number }): number {
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

const SPAN = 2000;
const ranges: { from: number, to: number }[] = [];
for (let i = 0; i < 200; i++) {
    ranges.push({ from: i * SPAN, to: (i + 1) * SPAN });
}

const t0 = performance.now();
const counts = await parallel.map(ranges, countPrimes);
let total = 0;
for (const c of counts) total += c;
const t1 = performance.now();

console.log(`workers: ${runtime.cpu.count}`);
console.log(`primes below ${200 * SPAN}: ${total}`);
console.log(`took ${Math.floor(t1 - t0)}ms`);
