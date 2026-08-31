import { odd } from "./odd.ts";
export function even(n: number) { if (n === 0) { return true; } return odd(n - 1); }
