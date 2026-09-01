// Regression: the minor GC left `obj_bases[1]` pointing at the detached
// nursery's dangling pointer, and only the JIT's inline bump allocation
// (which does not go through the Rust allocator's refresh) could observe
// it. Needs enough iterations to force a minor collection that promotes.
function f(n) {
  let s = 0;
  for (let i = 0; i < n; i++) {
    const o = { a: 1 };
    o.a = i; o.b = i + 1; o.c = i + 2;      // b,c are new -> transitions
    s = (s + o.a + o.b + o.c) % 1000003;
    const p = { x: 0, y: 0, z: 0 };
    p.x = i; p.y = i; p.z = "str";          // z becomes non-numeric
    s = (s + p.x + p.y) % 1000003;
  }
  return s;
}
console.log(f(200000));
