// Mandelbrot row counts: skewed per-row cost
function mandelRow(y: number): number {
    const H = 240, W = 320, MAXI = 500;
    const ci = (y / H) * 2 - 1;
    let inside = 0;
    for (let px = 0; px < W; px++) {
        const cr = (px / W) * 3 - 2;
        let zr = 0, zi = 0, i = 0;
        while (i < MAXI && zr * zr + zi * zi <= 4) {
            const t = zr * zr - zi * zi + cr;
            zi = 2 * zr * zi + ci;
            zr = t;
            i++;
        }
        if (i === MAXI) inside++;
    }
    return inside;
}
const rows: number[] = [];
for (let y = 0; y < 240; y++) rows.push(y);
const t0 = performance.now();
const counts = parallel.map(rows, mandelRow);
let total = 0;
for (const c of counts) total += c;
const t1 = performance.now();
console.log(`RESULT ${total}`);
console.log(`TIME_MS ${t1 - t0}`);
