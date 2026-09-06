//! runtime.fs error paths and platform-specific calls.
//!
//! The portable happy path is `tests/golden/fs.ts`, which also runs under
//! the differential JIT/GC matrix. What lives here is everything that must
//! not: rejections (a golden fixture must exit 0) and the Unix-only calls.

use std::path::PathBuf;
use std::process::Command;

fn write_case(name: &str, source: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tscore-fs-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join(name);
    std::fs::write(&f, source).unwrap();
    f
}

fn run(name: &str, source: &str) -> (bool, String, String) {
    let f = write_case(name, source);
    let out = Command::new(env!("CARGO_BIN_EXE_tscore"))
        .args(["run", f.to_str().unwrap(), "--no-stats-export"])
        .output()
        .expect("run tscore");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// A scratch directory this test owns, so a script that is meant to abort
/// before its own cleanup does not leak one.
fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join("tscore-fs-tests").join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run_ok(name: &str, source: &str) -> String {
    let (ok, stdout, stderr) = run(name, source);
    assert!(ok, "{name} failed: {stderr}");
    stdout
}

/// The documented contract: an I/O error rejects the promise, and the
/// rejection re-raises at the await with the OS message attached.
#[test]
fn missing_file_rejects_with_the_os_message() {
    let (ok, stdout, stderr) = run(
        "missing.ts",
        "const p = runtime.path.join(runtime.fs.tempDir(), \"tscore-definitely-absent\");\n\
         console.log(\"before\");\n\
         await runtime.fs.readFile(p);\n\
         console.log(\"unreachable\");\n",
    );
    assert!(!ok, "reading an absent file should fail");
    assert_eq!(stdout, "before\n");
    assert!(stderr.contains("fs.readFile"), "{stderr}");
    assert!(
        stderr.contains("tscore-definitely-absent"),
        "the path is missing from: {stderr}"
    );
}

/// Decoders take untrusted program input: bad input must raise, never
/// silently substitute a zero byte.
#[test]
fn byte_decoders_reject_bad_input() {
    for (name, expr) in [
        ("hex_odd", "runtime.bytes.fromHex(\"abc\")"),
        ("hex_digit", "runtime.bytes.fromHex(\"zz\")"),
        ("b64_len", "runtime.bytes.fromBase64(\"Zg=\")"),
        ("b64_char", "runtime.bytes.fromBase64(\"Zg!=\")"),
        ("arr_range", "runtime.bytes.fromArray([0, 256])"),
        ("arr_frac", "runtime.bytes.fromArray([1.5])"),
        ("arr_type", "runtime.bytes.fromArray([\"a\"])"),
        (
            "utf8",
            "runtime.bytes.decode(runtime.bytes.fromHex(\"ff\"))",
        ),
    ] {
        let (ok, _, stderr) = run(&format!("{name}.ts"), &format!("{expr};\n"));
        assert!(!ok, "{name}: expected a runtime error");
        assert!(
            stderr.contains("bytes."),
            "{name}: error does not name the call: {stderr}"
        );
    }
}

/// Each call hands back a directory this process just created.
#[test]
fn make_temp_dir_is_unique_and_real() {
    let out = run_ok(
        "temp.ts",
        "const a = await runtime.fs.makeTempDir();\n\
         const b = await runtime.fs.makeTempDir();\n\
         const st = await runtime.fs.stat(a);\n\
         console.log(`${a === b} ${st.isDir}`);\n\
         await runtime.fs.remove(a);\n\
         await runtime.fs.remove(b);\n",
    );
    assert_eq!(out, "false true\n");
}

/// stat follows a symlink, lstat does not — the distinction a tree walk
/// needs to avoid following links into a cycle.
#[cfg(unix)]
#[test]
fn symlink_readlink_and_lstat() {
    let out = run_ok(
        "symlink.ts",
        "const fs = runtime.fs;\n\
         const path = runtime.path;\n\
         const dir = await fs.makeTempDir();\n\
         const target = path.join(dir, \"target.txt\");\n\
         const link = path.join(dir, \"link.txt\");\n\
         await fs.writeFile(target, \"12345\");\n\
         await fs.symlink(target, link);\n\
         const s = await fs.stat(link);\n\
         const l = await fs.lstat(link);\n\
         console.log(`stat ${s.size} ${s.isFile} ${s.isSymlink}`);\n\
         console.log(`lstat ${l.isFile} ${l.isSymlink}`);\n\
         console.log(`readLink ${(await fs.readLink(link)) === target}`);\n\
         const entries = await fs.readDir(dir);\n\
         for (const e of entries) { console.log(`entry ${e.name} ${e.isSymlink}`); }\n\
         await fs.remove(dir);\n",
    );
    assert_eq!(
        out,
        "stat 5 true false\n\
         lstat false true\n\
         readLink true\n\
         entry link.txt true\n\
         entry target.txt false\n"
    );
}

#[cfg(unix)]
#[test]
fn chmod_is_observable_and_validated() {
    // mode 0 makes the file unreadable, so the read rejects. (Would not
    // hold as root, which ignores the permission bits.)
    let dir = scratch("chmod");
    let f = dir.join("f");
    let (ok, stdout, stderr) = run(
        "chmod.ts",
        &format!(
            "const fs = runtime.fs;\n\
             const f = \"{}\";\n\
             await fs.writeFile(f, \"x\");\n\
             await fs.chmod(f, 0);\n\
             console.log(\"chmodded\");\n\
             await fs.readFile(f);\n\
             console.log(\"unreachable\");\n",
            f.display()
        ),
    );
    assert!(!ok, "reading a mode-0 file should fail");
    assert_eq!(stdout, "chmodded\n");
    assert!(stderr.contains("fs.readFile"), "{stderr}");

    // 420 == 0o644: readable again
    let out = run_ok(
        "chmod_back.ts",
        &format!(
            "const fs = runtime.fs;\n\
             const f = \"{}\";\n\
             await fs.chmod(f, 420);\n\
             console.log(`now ${{await fs.readFile(f)}}`);\n",
            f.display()
        ),
    );
    assert_eq!(out, "now x\n");
    std::fs::remove_dir_all(&dir).unwrap();

    // an out-of-range mode is rejected before it reaches the syscall
    let (ok, _, stderr) = run(
        "chmod_bad.ts",
        "await runtime.fs.chmod(runtime.fs.tempDir(), 99999);\n",
    );
    assert!(!ok);
    assert!(stderr.contains("fs.chmod"), "{stderr}");
}
