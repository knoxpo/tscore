// Monte Carlo Value-at-Risk for a 5-stock portfolio over one trading month.
//
// Problem: "With 99% confidence, what is the most this portfolio can lose
// over the next 21 trading days?"  Under a one-factor model (a shared
// market shock plus a stock-specific shock per name) there is no closed
// form for the basket, so we simulate: draw daily returns for every stock,
// roll them forward 21 days, and read the loss quantiles off hundreds of
// thousands of independent scenarios.  Independent scenarios are exactly
// the shape of work parallel.map is for.
//
// The file runs unchanged on tscore and on Node.  Node has no parallel.map,
// so it falls back to a serial loop:
//   tscore run examples/portfolio_var.ts --workers 8
//   node examples/portfolio_var.ts

const SCENARIOS = 400000;
const BATCHES = 128;                  // parallel.map jobs
const DAYS = 21;
const SQRT_DT = Math.sqrt(1 / 252);
const BINS = 2000;                    // histogram over [-50%, +50%] portfolio return
const LO = -0.5;
const WIDTH = 1.0 / BINS;

// Portfolio: weight, annual drift, beta to the market factor, and the
// idiosyncratic volatility left after the market factor is removed
// (sqrt(sigma^2 - (beta * MARKET_SIGMA)^2)).
const MARKET_SIGMA = 0.16;
const W0 = 0.30, W1 = 0.25, W2 = 0.20, W3 = 0.15, W4 = 0.10;   // ACME BOLT CRUX DYNE EVER
const MU0 = 0.08, MU1 = 0.12, MU2 = 0.05, MU3 = 0.15, MU4 = 0.03;
const B0 = 0.80, B1 = 1.20, B2 = 0.50, B3 = 1.50, B4 = 0.30;
const E0 = Math.sqrt(0.25 * 0.25 - B0 * B0 * MARKET_SIGMA * MARKET_SIGMA);
const E1 = Math.sqrt(0.40 * 0.40 - B1 * B1 * MARKET_SIGMA * MARKET_SIGMA);
const E2 = Math.sqrt(0.18 * 0.18 - B2 * B2 * MARKET_SIGMA * MARKET_SIGMA);
const E3 = Math.sqrt(0.55 * 0.55 - B3 * B3 * MARKET_SIGMA * MARKET_SIGMA);
const E4 = Math.sqrt(0.12 * 0.12 - B4 * B4 * MARKET_SIGMA * MARKET_SIGMA);

// One scenario: roll the five stocks forward DAYS days, return the
// portfolio's return.  Pure f64 arithmetic on one annotated argument, no
// arrays, no calls: this qualifies for the typed JIT tier, so the whole
// function runs unboxed in FP registers.  Random numbers come from a
// Park-Miller LCG (exact in f64) and a 12-uniform sum for the normal draw,
// so the result is deterministic and identical on every engine and core
// count.
function scenarioReturn(seed: number): number {
    let x = seed % 2147483647;
    if (x <= 0) x = x + 2147483646;
    let p0 = 1, p1 = 1, p2 = 1, p3 = 1, p4 = 1;
    for (let d = 0; d < DAYS; d++) {
        let zm = -6, z0 = -6, z1 = -6, z2 = -6, z3 = -6, z4 = -6;
        for (let k = 0; k < 12; k++) {
            x = (x * 48271) % 2147483647; zm += x / 2147483647;
            x = (x * 48271) % 2147483647; z0 += x / 2147483647;
            x = (x * 48271) % 2147483647; z1 += x / 2147483647;
            x = (x * 48271) % 2147483647; z2 += x / 2147483647;
            x = (x * 48271) % 2147483647; z3 += x / 2147483647;
            x = (x * 48271) % 2147483647; z4 += x / 2147483647;
        }
        const market = MARKET_SIGMA * zm * SQRT_DT;
        p0 = p0 * (1 + MU0 / 252 + B0 * market + E0 * z0 * SQRT_DT);
        p1 = p1 * (1 + MU1 / 252 + B1 * market + E1 * z1 * SQRT_DT);
        p2 = p2 * (1 + MU2 / 252 + B2 * market + E2 * z2 * SQRT_DT);
        p3 = p3 * (1 + MU3 / 252 + B3 * market + E3 * z3 * SQRT_DT);
        p4 = p4 * (1 + MU4 / 252 + B4 * market + E4 * z4 * SQRT_DT);
    }
    return W0 * p0 + W1 * p1 + W2 * p2 + W3 * p3 + W4 * p4 - 1;
}

function zeros(n: number): number[] {
    const out: number[] = [];
    for (let i = 0; i < n; i++) out.push(0);
    return out;
}

// One parallel.map job: `count` scenarios bucketed into a histogram.
function simulateBatch(seed: number, count: number, hist: number[]): number[] {
    for (let s = 0; s < count; s++) {
        const ret = scenarioReturn(seed * count + s + 1);
        let b = ((ret - LO) / WIDTH) | 0;       // truncation == floor for ret >= LO
        if (ret < LO) b = 0;
        if (b >= BINS) b = BINS - 1;
        hist[b] = hist[b] + 1;
    }
    return hist;
}

// --- engine shim: identical source on tscore and Node -------------------
const native = typeof parallel !== "undefined";
const cores = native ? runtime.cpu.count : 1;
async function pmap(items: { seed: number, count: number }[], f: any): Promise<number[][]> {
    if (native) return await parallel.map(items, f);
    const out: number[][] = [];
    for (const it of items) out.push(f(it));   // Node: serial fallback
    return out;
}
// ------------------------------------------------------------------------

const batches: { seed: number, count: number }[] = [];
for (let b = 0; b < BATCHES; b++) batches.push({ seed: b, count: SCENARIOS / BATCHES });

const t0 = performance.now();
const hists = await pmap(batches, (job: { seed: number, count: number }) =>
    simulateBatch(job.seed, job.count, zeros(BINS)));
const t1 = performance.now();

// merge the histograms, then read the quantiles off the cumulative counts
const total: number[] = zeros(BINS);
for (const h of hists) for (let i = 0; i < BINS; i++) total[i] += h[i];

function quantileLoss(q: number): number {           // loss at the q-th worst scenario
    let seen = 0;
    for (let i = 0; i < BINS; i++) {
        seen += total[i];
        if (seen >= SCENARIOS * (1 - q)) return -(LO + (i + 0.5) * WIDTH);
    }
    return -LO;
}
function expectedShortfall(q: number): number {      // mean loss beyond the q-quantile
    const cutoff = SCENARIOS * (1 - q);
    let seen = 0, lossSum = 0;
    for (let i = 0; i < BINS && seen < cutoff; i++) {
        const take = Math.min(total[i], cutoff - seen);
        lossSum += take * -(LO + (i + 0.5) * WIDTH);
        seen += take;
    }
    return lossSum / cutoff;
}
let mean = 0;
for (let i = 0; i < BINS; i++) mean += total[i] * (LO + (i + 0.5) * WIDTH);
mean = mean / SCENARIOS;

const pct = (v: number) => `${Math.floor(v * 10000) / 100}%`;
console.log(`portfolio: ACME 30%  BOLT 25%  CRUX 20%  DYNE 15%  EVER 10%`);
console.log(`scenarios: ${SCENARIOS}  horizon: ${DAYS} days  engine: ${native ? "tscore" : "node"}  cores: ${cores}`);
console.log(`expected 1-month return: ${pct(mean)}`);
console.log(`VaR 95%: ${pct(quantileLoss(0.95))}   VaR 99%: ${pct(quantileLoss(0.99))}`);
console.log(`expected shortfall 99%: ${pct(expectedShortfall(0.99))}`);
console.log(`simulation took ${Math.floor(t1 - t0)}ms`);
