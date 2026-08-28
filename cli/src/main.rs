//! tscore — TypeScript Core CLI.
//!
//! Usage:
//!   tscore run <file.ts> [--dump-bytecode] [--workers N]

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut file = None;
    let mut dump = false;
    let mut workers: Option<usize> = None;
    let mut it = args.iter();
    match it.next().map(String::as_str) {
        Some("run") => {}
        Some("--version") | Some("-V") => {
            println!("tscore {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        _ => {
            eprintln!("usage: tscore run <file.ts> [--dump-bytecode] [--workers N]");
            return ExitCode::from(2);
        }
    }
    while let Some(a) = it.next() {
        match a.as_str() {
            "--dump-bytecode" => dump = true,
            "--workers" => {
                workers = it.next().and_then(|n| n.parse().ok());
                if workers.is_none() {
                    eprintln!("--workers needs a number");
                    return ExitCode::from(2);
                }
            }
            f if !f.starts_with('-') && file.is_none() => file = Some(f.to_string()),
            other => {
                eprintln!("unknown argument: {other}");
                return ExitCode::from(2);
            }
        }
    }
    let Some(file) = file else {
        eprintln!("usage: tscore run <file.ts>");
        return ExitCode::from(2);
    };

    let source = match std::fs::read_to_string(&file) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("tscore: cannot read {file}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let chunk = match tsc_parser::compile(&source, &file) {
        Ok(c) => c,
        Err(e) => {
            let (line, col) = e.line_col(&source);
            eprintln!("{file}:{line}:{col}: {}", e.msg);
            return ExitCode::FAILURE;
        }
    };

    if dump {
        let mut out = String::new();
        tsc_ir::disassemble(&chunk.main, &mut out);
        print!("{out}");
        return ExitCode::SUCCESS;
    }

    let mut realm = tsr_realm::Realm::new();
    tsr_io::install(&mut realm);
    tss_parallel::install(&mut realm, workers);
    tsr_actor::install(&mut realm);

    match tsr_realm::interp::run_main(&mut realm, &chunk.main) {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            match e.span {
                Some(span) => {
                    let err = tsc_parser::CompileError {
                        msg: String::new(),
                        span_start: span,
                    };
                    let (line, col) = err.line_col(&source);
                    eprintln!("{file}:{line}:{col}: runtime error: {}", e.msg);
                }
                None => eprintln!("{file}: runtime error: {}", e.msg),
            }
            ExitCode::FAILURE
        }
    }
}
