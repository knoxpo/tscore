# M1 Language Subset (TS-M1)

The resolver **rejects** anything outside this list with a
`not supported in M1` diagnostic carrying a source span. Scope is enforced by
code, not discipline.

## In

- Declarations: `let`, `const`, `function`, arrow functions
- **Closures** with full lexical capture (required for `parallel.map` ergonomics)
- Values: `number` (f64 only), `boolean`, `string` (immutable), `null`,
  `undefined`, object literals (shapeless), array literals, functions
- Operators: all arithmetic, comparison (`==`/`===` fold to same op on the
  subset's value set), logical (short-circuit), bitwise (f64→i32 coercion),
  `typeof`, ternary, compound assignment, `++`/`--`
- Control flow: `if`/`else`, `while`, `for`, `for-of` (arrays only),
  `break`, `continue`, `return`
- Property access: dot + computed index; `arr.length`, `arr.push(x)`
- Template literals
- TypeScript annotations, `interface`, `type` — **parsed and stripped**, never checked
- Globals: `console.log`, `Math.{floor,sqrt,abs,min,max,imul}`,
  `Date.now()`, `performance.now()`, `parallel.map`, `parallel.for`

## Out (deferred)

`class` / prototypes / `this` / `new` — objects are plain maps ·
`try`/`catch`/`throw` (runtime error aborts the task; removes unwinding from
the interpreter) · `async`/`await`, Promises, generators, iterator protocol ·
`Map`/`Set`, `Symbol`, regex, getters/setters · destructuring, spread,
default params, `var`, `with`, `eval` · modules (single-file entry only).

This subset is exactly enough for honest CPU benchmarks: FNV-style hashing,
prime trial division, Mandelbrot — f64 math, bitwise ops, arrays, closures.
