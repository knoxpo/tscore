// exercises the integer lane's exits: overflow past i32, values that are
// not integers on loop entry, negatives, and the zero-remainder path
function chain(n, start, mul, add, m) {
    let s = start;
    for (let i = 0; i < n; i++) s = (s * mul + add + i) % m;
    return s;
}
let out = "";
out = out + chain(200, 0, 31, 0, 1000003) + ",";
out = out + chain(200, 0.5, 31, 0, 1000003) + ",";        // non-integral entry
out = out + chain(200, -7, 31, 3, 1000003) + ",";          // negatives
out = out + chain(200, 1e15, 31, 0, 1000003) + ",";        // overflows i32 immediately
out = out + chain(200, 2147483647, 3, 1, 1000003) + ",";   // straddles i32 max
out = out + chain(200, 0, 1000003, 0, 1000003) + ",";      // remainder always 0
out = out + chain(200, 5, 2, 0, 7) + ",";                  // small modulus
out = out + chain(200, -5, 2, 0, -7) + ",";                // negative modulus
out = out + chain(200, 0, 1, 1, 2) + ",";                  // divisor 2
out = out + chain(200, 12345, 7, 11, 1024) + ",";          // power of two
console.log(out);
