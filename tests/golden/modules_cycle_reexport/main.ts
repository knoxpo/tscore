import { readB } from "./a.ts";
import { aVal } from "./b.ts";
console.log(readB());
console.log("re-exported through the cycle:", aVal);
