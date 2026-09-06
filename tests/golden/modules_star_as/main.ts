import { math, own } from "./lib.ts";
console.log(own, math.pi);
math.bump();
math.bump();
console.log("hits:", math.hits);
console.log("meta:", import.meta.url.length > 7);
