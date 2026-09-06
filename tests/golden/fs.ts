// runtime.fs / runtime.bytes / runtime.path — portable happy path.
// The scratch directory comes from makeTempDir, so this fixture does not
// depend on the working directory and cannot collide with a parallel run.
// Nothing non-reproducible is printed: no paths, no modifiedMs.

const fs = runtime.fs;
const bytes = runtime.bytes;
const path = runtime.path;

const dir = await fs.makeTempDir();
const file = path.join(dir, "a.txt");

// ---- text round trip ----
await fs.writeFile(file, "hello");
await fs.appendFile(file, "world");
console.log(`read: ${await fs.readFile(file)}`);

const st = await fs.stat(file);
console.log(`stat: ${st.size} ${st.isFile} ${st.isDir} ${st.isSymlink}`);

// ---- directory listing ----
await fs.mkdir(path.join(dir, "sub"));
await fs.writeFile(path.join(dir, "b.txt"), "b");
const entries = await fs.readDir(dir);
for (const e of entries) {
  console.log(`entry: ${e.name} file=${e.isFile} dir=${e.isDir} link=${e.isSymlink}`);
}

// ---- exists / rename / copy / truncate ----
console.log(`exists: ${await fs.exists(file)} ${await fs.exists(path.join(dir, "nope"))}`);
await fs.rename(file, path.join(dir, "renamed.txt"));
console.log(`renamed: ${await fs.exists(file)} ${await fs.exists(path.join(dir, "renamed.txt"))}`);
await fs.copy(path.join(dir, "renamed.txt"), path.join(dir, "copy.txt"));
console.log(`copied: ${await fs.readFile(path.join(dir, "copy.txt"))}`);
await fs.truncate(path.join(dir, "copy.txt"), 5);
console.log(`truncated: ${await fs.readFile(path.join(dir, "copy.txt"))}`);

// ---- binary round trip ----
const raw = bytes.fromArray([0, 15, 16, 255, 65, 66]);
const binFile = path.join(dir, "bin");
await fs.writeBytes(binFile, raw);
const back = await fs.readBytes(binFile);
console.log(`bytes rt: ${bytes.equals(raw, back)} ${bytes.size(back)}`);

// ---- bytes surface ----
console.log(`at: ${bytes.at(back, 0)} ${bytes.at(back, 3)} ${bytes.at(back, 99)}`);
console.log(`hex: ${bytes.toHex(back)}`);
console.log(`fromHex: ${bytes.equals(bytes.fromHex("000f10ff4142"), back)}`);
console.log(`b64: ${bytes.toBase64(bytes.fromString("foobar"))} ${bytes.toBase64(bytes.fromString("f"))}`);
console.log(`fromB64: ${bytes.toString(bytes.fromBase64("Zm9vYmE="))}`);
console.log(`slice: ${bytes.toHex(bytes.slice(back, 4, 6))}`);
console.log(`concat: ${bytes.toHex(bytes.concat(bytes.fromHex("ab"), bytes.fromHex("cd")))}`);
console.log(`indexOf: ${bytes.indexOf(back, bytes.fromHex("10ff"))} ${bytes.indexOf(back, bytes.fromHex("dead"))}`);
console.log(`decode: ${bytes.decode(bytes.fromString("héllo"))}`);
const nums = bytes.toArray(bytes.fromHex("00ff41"));
let numsStr = "";
for (const n of nums) { numsStr = numsStr + n + " "; }
console.log(`toArray: ${nums.length} ${numsStr}`);

// ---- path surface ----
console.log(`join: ${path.join("a", "b", "../c")} ${path.join("/x", "y")}`);
console.log(`dirname: ${path.dirname("a/b")} ${path.dirname("a")} ${path.dirname("/a")}`);
console.log(`basename: ${path.basename("a/b.txt")} ${path.basename("/")}`);
console.log(`extname: ${path.extname("a/b.txt")} [${path.extname(".gitignore")}]`);
console.log(`normalize: ${path.normalize("a//b/./c/../d")} ${path.normalize("/../x")}`);
console.log(`isAbsolute: ${path.isAbsolute("/a")} ${path.isAbsolute("a")}`);

// ---- cleanup ----
await fs.remove(dir);
console.log(`cleaned: ${await fs.exists(dir)}`);
