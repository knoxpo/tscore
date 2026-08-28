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
        if path.extension().is_none_or(|e| e != "ts") {
            continue;
        }
        let expected = std::fs::read_to_string(path.with_extension("out")).unwrap();
        for workers in ["1", "4"] {
            let got = run(&path, workers);
            assert_eq!(
                got, expected,
                "{} diverged at --workers {workers}",
                path.display()
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
