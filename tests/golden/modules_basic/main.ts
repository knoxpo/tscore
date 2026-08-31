import twice, { greeting, counter, bump } from "./util.ts";
import * as u from "./util.ts";
console.log(greeting, twice(21));
console.log("before:", counter);
bump();
bump();
console.log("after:", counter, u.counter);
