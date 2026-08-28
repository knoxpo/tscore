let keep = { hits: 0 };
for (let i = 0; i < 300000; i++) {
    const garbage = { tag: `x${i}`, arr: [i, i + 1] };
    if (garbage.arr[0] % 100000 === 0) keep.hits = keep.hits + 1;
}
console.log("survived:", keep.hits);
