// The bounds check may reuse the length the loop compare just read, so
// anything that can change a length between the two must drop that fact.
// Growth mid-body is the reachable case (arrays cannot shrink here).
function grow(n) {
  let s = 0;
  for (let r = 0; r < n; r++) {
    const a = [1, 2, 3];
    for (let i = 0; i < a.length; i++) {
      if (a.length < 12) a.push(i * 2);   // between the Len and the index
      s = (s + a[i]) % 1000003;
    }
  }
  return s;
}
// two arrays alternating at one site: the cached pair names a vreg, and
// the wrong array's length must never be reused
function two(n) {
  const a = [1, 2, 3, 4, 5, 6, 7, 8];
  const b = [9, 8];
  let s = 0;
  for (let r = 0; r < n; r++) {
    const x = r % 2 === 0 ? a : b;
    for (let i = 0; i < x.length; i++) s = (s + x[i]) % 1000003;
  }
  return s;
}
// the length vreg overwritten between the read and the index
function clobber(n) {
  const a = [4, 5, 6, 7];
  let s = 0;
  for (let r = 0; r < n; r++) {
    let len = a.length;
    for (let i = 0; i < len; i++) {
      len = a.length;
      s = (s + a[i]) % 1000003;
    }
  }
  return s;
}
console.log(grow(2000), two(2000), clobber(2000));
