// FNV-1a-style hashing over xorshift streams: pure f64 + bitwise
function hashStream(seed: number): number {
    let x = seed | 0;
    let h = 2166136261 | 0;
    for (let i = 0; i < 300000; i++) {
        x = (x ^ (x << 13)) | 0;
        x = (x ^ (x >>> 17)) | 0;
        x = (x ^ (x << 5)) | 0;
        h = Math.imul(h ^ (x & 255), 16777619);
    }
    return h >>> 0;
}
const seeds: number[] = [];
for (let i = 1; i <= 256; i++) seeds.push(i);
const t0 = performance.now();
const hashes = parallel.map(seeds, hashStream);
let checksum = 0;
for (const h of hashes) checksum = (checksum + h) % 4294967296;
const t1 = performance.now();
console.log(`RESULT ${checksum}`);
console.log(`TIME_MS ${t1 - t0}`);
