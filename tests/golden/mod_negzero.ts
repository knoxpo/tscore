// The -0 branch in a constant-divisor modulo: a zero remainder of a
// negative dividend is -0, which no integer register can hold, so it
// deopts. Every other sign/remainder combination stays in the fast path.
function f(n) {
  let out = "";
  for (let r = 0; r < n; r++) {
    for (let i = -6; i <= 6; i++) {
      const m = i % 3;
      if (r === n - 1) out = out + (1 / m === -Infinity ? "-0" : "" + m) + " ";
    }
  }
  return out;
}
function g(n) { let s = 0; for (let r = 0; r < n; r++) for (let i = -50; i < 50; i++) s += (i % 7) + (i % 1000); return s; }
function h(n) { let s = 0; for (let r = 0; r < n; r++) { let i = -1000; while (i < 1000) { s += i % 250; i = i + 1; } } return s; }
console.log(f(3000));
console.log(g(5000), h(500));
