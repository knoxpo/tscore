export const name = "worker";
export let tick = 0;
export function bump() { tick = tick + 1; }
