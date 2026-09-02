// `%` carries the dividend's sign, including -0 for a zero remainder.
// The emitter picks between a branchless sign fixup and a branch on the
// divisor's size, so both sides of that threshold are exercised here.
// 1/x distinguishes -0 from 0.
const vs = [0, 1, -1, 7, -7, 14, -14, 13, -13, 6, -6, 1000, -1000, 999, -999,
            2147483647, -2147483648];
const ds = [2, 7, 13, 16, 17, 50, 1000, 1000003];
let out = "";
for (let i = 0; i < vs.length; i++) {
    for (let j = 0; j < ds.length; j++) {
        const r = vs[i] % ds[j];
        const rn = vs[i] % -ds[j];
        out = out + r + "," + (1 / r) + "," + rn + "," + (1 / rn) + ";";
    }
}
console.log(out);
