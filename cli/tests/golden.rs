//! Golden-output conformance: every tests/golden/*.ts must print exactly
//! its .out file — and parallel programs must print it at ANY worker count
//! (determinism guarantee).

use std::path::PathBuf;
use std::process::Command;

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tests/golden")
}

fn run(file: &PathBuf, workers: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_tscore"))
        .args(["run", file.to_str().unwrap(), "--workers", workers])
        .output()
        .expect("run tscore");
    assert!(
        out.status.success(),
        "{} failed: {}",
        file.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn golden_outputs_match_at_all_worker_counts() {
    let mut checked = 0;
    for entry in std::fs::read_dir(golden_dir()).unwrap() {
        let path = entry.unwrap().path();
        // directory fixture: <name>/main.ts (entry, may import siblings)
        // + <name>/expected.out
        let (entry_file, expected_file) = if path.is_dir() {
            let main = path.join("main.ts");
            let exp = path.join("expected.out");
            if !main.exists() || !exp.exists() {
                continue;
            }
            (main, exp)
        } else {
            if path.extension().is_none_or(|e| e != "ts") {
                continue;
            }
            (path.clone(), path.with_extension("out"))
        };
        let expected = std::fs::read_to_string(&expected_file).unwrap();
        for workers in ["1", "4"] {
            let got = run(&entry_file, workers);
            assert_eq!(
                got, expected,
                "{} diverged at --workers {workers}",
                entry_file.display()
            );
        }
        checked += 1;
    }
    assert!(checked >= 3, "golden fixtures missing");
}

#[test]
fn subset_violation_has_span_diagnostic() {
    let dir = std::env::temp_dir().join("tscore-golden");
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("bad.ts");
    std::fs::write(&f, "const a = 1;\nclass Nope {}\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_tscore"))
        .args(["run", f.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("bad.ts:2:1"), "no span in: {err}");
    assert!(err.contains("not supported in M1"), "{err}");
}

#[test]
fn runtime_error_has_span() {
    let dir = std::env::temp_dir().join("tscore-golden");
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("rterr.ts");
    std::fs::write(&f, "const x = 1;\nconsole.log(missingGlobal);\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_tscore"))
        .args(["run", f.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("rterr.ts:2:") && err.contains("missingGlobal is not defined"),
        "{err}"
    );
}

// ---- M3 structured concurrency: failure paths ----

fn run_expect_fail(source: &str) -> String {
    let dir = std::env::temp_dir().join("tscore-golden");
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join(format!("m3-{:x}.ts", source.len() * 31 + source.as_bytes()[0] as usize));
    std::fs::write(&f, source).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_tscore"))
        .args(["run", f.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out.status.success(), "expected failure");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn child_error_cancels_siblings_and_propagates() {
    let start = std::time::Instant::now();
    let err = run_expect_fail(
        "task.scope((scope) => {\n\
             scope.spawn(() => { let n = 0; while (true) { n++; } });\n\
             scope.spawn(() => missingGlobal);\n\
         });\n",
    );
    assert!(err.contains("task scope failed: missingGlobal is not defined"), "{err}");
    // the spinning sibling was cancelled, not run to (never) completion
    assert!(start.elapsed().as_secs() < 10, "sibling was not cancelled");
}

#[test]
fn scope_timeout_cancels_children() {
    let start = std::time::Instant::now();
    let err = run_expect_fail(
        "task.scope((scope) => {\n\
             scope.spawn(() => { let n = 0; while (true) { n++; } });\n\
         }, { timeout: 300 });\n",
    );
    assert!(err.contains("scope timed out after 300ms"), "{err}");
    let secs = start.elapsed().as_secs_f64();
    assert!(secs < 8.0, "timeout did not fire promptly: {secs}s");
}
