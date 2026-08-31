const m = await import("./lazy.ts");
console.log(m.label);
m.hit();
m.hit();
console.log("hits:", m.hits);
const m2 = await import("./lazy.ts");
console.log("cached:", m2.hits);
