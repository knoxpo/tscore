//! M6 lazy-compilation conformance: deferred fills, deferred subset
//! errors, the parallel frontend, and tier interactions must all produce
//! the same observable behavior as the fully eager pipeline.

use std::path::PathBuf;
use std::process::Command;

fn write_case(name: &str, source: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tscore-lazy");
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join(name);
    std::fs::write(&f, source).unwrap();
    f
}

fn run_with(file: &PathBuf, envs: &[(&str, &str)], workers: &str) -> (bool, String, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tscore"));
    cmd.args(["run", file.to_str().unwrap(), "--workers", workers]);
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("run tscore");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Every mode a program can compile under; all must agree.
const MODES: [&[(&str, &str)]; 4] = [
    &[],
    &[("TSC_NO_LAZY", "1")],
    &[("TSC_NO_PARALLEL_FRONTEND", "1")],
    &[("TSC_NO_LAZY", "1"), ("TSC_NO_PARALLEL_FRONTEND", "1")],
];

fn assert_all_modes_agree(name: &str, source: &str) -> String {
    let f = write_case(name, source);
    let mut expected: Option<String> = None;
    for envs in MODES {
        let (ok, stdout, stderr) = run_with(&f, envs, "2");
        assert!(ok, "{name} failed under {envs:?}: {stderr}");
        match &expected {
            None => expected = Some(stdout),
            Some(e) => assert_eq!(&stdout, e, "{name} diverged under {envs:?}"),
        }
    }
    expected.unwrap()
}

#[test]
fn closures_and_mutual_recursion_across_lazy_boundary() {
    let out = assert_all_modes_agree(
        "closures.ts",
        "function counterFactory() {\n\
             let n = 0;\n\
             return function bump() { n = n + 1; return twice(n); };\n\
         }\n\
         function twice(x) { return half(x * 4); }\n\
         function half(x) { return x / 2; }\n\
         function even(x) { if (x === 0) { return true; } return odd(x - 1); }\n\
         function odd(x) { if (x === 0) { return false; } return even(x - 1); }\n\
         const c = counterFactory();\n\
         c(); c();\n\
         console.log(`RESULT ${c()} ${even(10)}`);\n",
    );
    assert_eq!(out, "RESULT 6 true\n");
}

#[test]
fn async_bodies_fill_lazily() {
    let out = assert_all_modes_agree(
        "async.ts",
        "async function leaf(x) { return x * 2; }\n\
         async function chain(n) {\n\
             if (n === 0) { return 0; }\n\
             return (await chain(n - 1)) + (await leaf(1));\n\
         }\n\
         console.log(`RESULT ${await chain(20)}`);\n",
    );
    assert_eq!(out, "RESULT 40\n");
}

#[test]
fn parallel_frontend_bundle_matches_serial() {
    // fast-path eligible: only top-level functions + bare statements
    let mut src = String::new();
    for i in 0..24 {
        src.push_str(&format!(
            "function f{i}(a) {{ const k = a + {i}; if (k % 3 === 0) {{ return f{}(k); }} return k; }}\n",
            (i + 1) % 24
        ));
    }
    src.push_str("console.log(`RESULT ${f0(1) + f7(2)}`);\n");
    let out = assert_all_modes_agree("bundle.ts", &src);
    assert!(out.starts_with("RESULT "), "{out}");
}

#[test]
fn proto_filled_through_worker_boundary() {
    // the callback proto crosses to pool workers (force-fill at the
    // portable boundary) and is also called on the main thread afterwards
    let out = assert_all_modes_agree(
        "boundary.ts",
        "function work(range) { return range.a * 10 + range.b; }\n\
         const items = [];\n\
         for (let i = 0; i < 32; i++) { items.push({ a: i, b: i + 1 }); }\n\
         const rs = await parallel.map(items, work);\n\
         let sum = 0;\n\
         for (const r of rs) { sum = sum + r; }\n\
         console.log(`RESULT ${sum + work({ a: 1, b: 1 })}`);\n",
    );
    assert!(out.starts_with("RESULT "), "{out}");
}

#[test]
fn lazy_proto_tiers_up() {
    // hot loop inside a lazily-filled function must OSR/tier-up cleanly;
    // forced-low thresholds vs no-JIT must agree
    let f = write_case(
        "tier.ts",
        "function hot(n) {\n\
             let s = 0;\n\
             for (let i = 0; i < n; i++) { s = (s * 31 + i) % 1000003; }\n\
             return s;\n\
         }\n\
         console.log(`RESULT ${hot(200000)}`);\n",
    );
    let (ok1, jit, err1) = run_with(
        &f,
        &[("TSC_JIT_THRESHOLD", "1"), ("TSC_OSR_THRESHOLD", "10")],
        "1",
    );
    assert!(ok1, "{err1}");
    let (ok2, nojit, err2) = run_with(&f, &[("TSC_NO_JIT", "1")], "1");
    assert!(ok2, "{err2}");
    assert_eq!(jit, nojit);
}

#[test]
fn statement_subset_error_reports_at_startup_despite_lazy() {
    // the startup subset scan catches statement-level violations even in
    // never-called lazy bodies — matching pre-M6 semantics
    let f = write_case(
        "scan-err.ts",
        "function neverCalled() { class Nope {} }\n\
         console.log(\"RESULT ok\");\n",
    );
    let (ok, stdout, stderr) = run_with(&f, &[], "1");
    assert!(!ok, "scan should reject at startup");
    assert_eq!(stdout, "");
    assert!(stderr.contains("scan-err.ts:1:26"), "no span in: {stderr}");
    assert!(stderr.contains("not supported in M1: class"), "{stderr}");
}

#[test]
fn expression_subset_error_defers_to_first_call() {
    // expression-level rejections are the emitter's business — for a lazy
    // body that means first call, with the original span (the safety net)
    let f = write_case(
        "deferred-err.ts",
        "function fine() { return 1; }\n\
         function bad(x) { return x in x; }\n\
         console.log(`RESULT ${fine()}`);\n\
         bad(1);\n",
    );
    let (ok, stdout, stderr) = run_with(&f, &[], "1");
    assert!(!ok, "bad() should fail");
    // code before the first call ran
    assert_eq!(stdout, "RESULT 1\n");
    assert!(
        stderr.contains("deferred-err.ts:2:"),
        "no span in: {stderr}"
    );
    assert!(stderr.contains("not supported in M1"), "{stderr}");
    // eager mode reports the same error before anything runs
    let (ok_e, stdout_e, stderr_e) = run_with(&f, &[("TSC_NO_LAZY", "1")], "1");
    assert!(!ok_e);
    assert_eq!(stdout_e, "");
    assert!(stderr_e.contains("not supported in M1"), "{stderr_e}");

    // never-called: the expression-level violation stays silent
    let f2 = write_case(
        "silent-expr.ts",
        "function neverCalled(x) { return x in x; }\n\
         console.log(\"RESULT ok\");\n",
    );
    let (ok2, stdout2, stderr2) = run_with(&f2, &[], "1");
    assert!(ok2, "{stderr2}");
    assert_eq!(stdout2, "RESULT ok\n");
}
