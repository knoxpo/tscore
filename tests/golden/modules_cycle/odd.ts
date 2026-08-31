import { even } from "./even.ts";
export function odd(n: number) { if (n === 0) { return false; } return even(n - 1); }
