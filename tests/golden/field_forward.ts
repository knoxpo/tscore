// aliasing hazards for store-to-load forwarding
function mutate(o) { o.f = 99; return 0; }
function main() {
    const out = [];
    // 1. call between store and load must not be forwarded
    const a = { f: 1 };
    a.f = 5; mutate(a); out.push(a.f);
    // 2. indexed store may alias the same property
    const b = { f: 1 };
    const k = "f";
    b.f = 5; b[k] = 7; out.push(b.f);
    // 3. value register reassigned between store and load
    let v = 3;
    const c = { f: 0 };
    c.f = v; v = 100; out.push(c.f);
    // 4. object register reassigned between store and load
    let o = { f: 1 };
    const other = { f: 2 };
    o.f = 50; o = other; out.push(o.f);
    // 5. two different registers aliasing one object
    const d = { f: 1 };
    const e = d;
    d.f = 11; out.push(e.f);
    // 6. store then load across a branch
    const g = { f: 1 };
    g.f = 20;
    if (out.length > 0) { g.f = 30; }
    out.push(g.f);
    // 7. new field via shape transition, then read back
    const h = { x: 1 };
    h.y = 42; out.push(h.y + h.x);
    // 8. the hot shape: compound assignment reading itself back
    const p = { z: 4 };
    for (let i = 0; i < 3; i++) { p.z = p.z + i; out.push(p.z % 7); }
    // 9. different keys must not cross-forward
    const q = { m: 1, n: 2 };
    q.m = 8; out.push(q.n);
    let s = "";
    for (let i = 0; i < out.length; i++) s = s + out[i] + ",";
    return s;
}
console.log(main());
