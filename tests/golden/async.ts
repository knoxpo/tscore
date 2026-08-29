// interleaved timers + async chains + await-parallel under GC pressure
async function work(label: string, ms: number): Promise<string> {
    await sleep(ms);
    return label;
}
const slow = work("slow", 60);
const fast = work("fast", 10);
console.log("order:", await fast, await slow);

async function fib(n: number): Promise<number> {
    if (n < 2) return n;
    return (await fib(n - 1)) + (await fib(n - 2));
}
console.log("async fib:", await fib(12));

const doubled = await parallel.map([1, 2, 3, 4], async (x: number) => {
    await sleep(5);
    return x * 2;
});
console.log("async map:", doubled);
