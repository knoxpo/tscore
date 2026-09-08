// A bounds check may only go when the loop compare proved `i < len` for
// *this* array and `i` is provably non-negative.
// bounded by a different array's length: the index runs past this one
function cross(n) {
  const a = [1, 2, 3];
  const b = [1, 2, 3, 4, 5, 6, 7, 8];
  let s = 0;
  for (let r = 0; r < n; r++)
    for (let i = 0; i < b.length; i++) s = (s + (a[i] === undefined ? 0 : a[i])) % 1000003;
  return s;
}
// `<=` reaches one past the end
function inclusive(n) {
  const a = [1, 2, 3];
  let s = 0;
  for (let r = 0; r < n; r++)
    for (let i = 0; i <= a.length; i++) s = (s + (a[i] === undefined ? 0 : a[i])) % 1000003;
  return s;
}
// the index is derived, and goes negative
function derived(n) {
  const a = [1, 2, 3, 4];
  let s = 0;
  for (let r = 0; r < n; r++) {
    for (let i = 0; i < a.length; i++) {
      const j = i - 2;
      s = (s + (a[j] === undefined ? 0 : a[j])) % 1000003;
    }
  }
  return s;
}
// counting down: not an increment by a positive constant
function down(n) {
  const a = [5, 6, 7, 8];
  let s = 0;
  for (let r = 0; r < n; r++) {
    let i = a.length - 1;
    while (i >= 0) { s = (s + a[i]) % 1000003; i = i - 1; }
  }
  return s;
}
// the plain case that should lose its check
function plain(n) {
  const a = [1, 2, 3, 4, 5, 6, 7, 8];
  let s = 0;
  for (let r = 0; r < n; r++)
    for (let i = 0; i < a.length; i++) s = (s + a[i]) % 1000003;
  return s;
}
console.log(cross(3000), inclusive(3000), derived(3000), down(3000), plain(3000));
