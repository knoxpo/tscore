//! tsc-parser
//!
//! TypeScript → bytecode: OXC parse (types stripped), capture analysis,
//! single-pass emission. The M1 subset gate lives in the emitter — anything
//! outside TS-M1 fails here with a source span, never at runtime.

mod emit;
mod resolve;
mod scan;
mod split;

use tsc_ast::oxc_allocator::Allocator;
use tsc_ir::Chunk;

#[derive(Debug)]
pub struct CompileError {
    pub msg: String,
    pub span_start: u32,
}

impl CompileError {
    /// 1-based (line, column) of the error in `source`.
    pub fn line_col(&self, source: &str) -> (usize, usize) {
        let upto = &source[..(self.span_start as usize).min(source.len())];
        let line = upto.matches('\n').count() + 1;
        let col = upto.chars().rev().take_while(|&c| c != '\n').count() + 1;
        (line, col)
    }
}

/// Parallel frontend (M6e): when the file is a flat bundle of top-level
/// functions (no top-level let/const/var/class — nothing capturable but
/// hoisted function names), hollow the bodies for the main parse and
/// resolve the real bodies on worker threads. Falls back to the serial
/// pipeline whenever the splitter is unsure.
fn compile_parallel(
    source: &str,
    source_name: &str,
    items: &[split::FnItem],
    phases: bool,
) -> Result<Chunk, CompileError> {
    let t0 = std::time::Instant::now();
    let hollowed = split::hollow(source, items);
    let allocator = Allocator::with_capacity(hollowed.len() * 2);
    let program = tsc_ast::parse(&allocator, &hollowed).map_err(|errs| {
        let e = &errs[0];
        CompileError { msg: format!("parse error: {}", e.msg), span_start: e.span_start }
    })?;
    // top-level function names + their binding-identifier spans
    use tsc_ast::oxc_ast::ast::Statement;
    let mut env_names: Vec<String> = Vec::with_capacity(items.len());
    let mut name_ids: Vec<(String, u32)> = Vec::with_capacity(items.len());
    for st in &program.body {
        if let Statement::FunctionDeclaration(f) = st {
            if let Some(id) = &f.id {
                env_names.push(id.name.to_string());
                name_ids.push((id.name.to_string(), id.span.start));
            }
        }
    }
    let t1 = std::time::Instant::now();

    // workers: parse + capture-resolve every real body in parallel.
    // Snippets are space-padded to their original offsets, so the capture
    // tables come back keyed by original-file spans.
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(16);
    let chunk = items.len().div_ceil(threads).max(1);
    type WorkerOut = (u32, Result<Vec<String>, (String, u32)>);
    let results: Vec<WorkerOut> = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for group in items.chunks(chunk) {
            let env_names = &env_names;
            handles.push(scope.spawn(move || {
                let mut out = Vec::with_capacity(group.len());
                for it in group {
                    // unpadded snippet: only capture NAMES come back, so
                    // snippet-local spans are fine (fills pad separately)
                    let text = &source[it.start as usize..it.end as usize];
                    let alloc = Allocator::with_capacity(text.len() * 8);
                    let Ok(prog) = tsc_ast::parse(&alloc, text) else {
                        // serial path will report the error properly
                        out.push((it.start, Ok(Vec::new())));
                        continue;
                    };
                    if let Some((msg, span)) = scan::subset_scan(&prog.body) {
                        out.push((it.start, Err((msg, span + it.start))));
                        continue;
                    }
                    let (_, _, mut caps) =
                        resolve::Resolver::run_seeded(&prog, env_names);
                    // key the result by the snippet's own top-level node
                    // (nested functions have their own entries)
                    let top_key = prog.body.iter().find_map(|st| match st {
                        Statement::FunctionDeclaration(f) => Some(f.span.start),
                        _ => None,
                    });
                    let caps = top_key
                        .and_then(|k| caps.remove(&k))
                        .unwrap_or_default();
                    out.push((it.start, Ok(caps)));
                }
                out
            }));
        }
        handles.into_iter().flat_map(|h| h.join().expect("frontend worker")).collect()
    });
    let t2 = std::time::Instant::now();

    // eager subset reporting: earliest error across workers and the
    // hollow rest (bodies are blanked there, so no double-reporting)
    let mut scan_err: Option<(String, u32)> = None;
    if let Some((msg, span)) = scan::subset_scan(&program.body) {
        scan_err = Some((msg, span));
    }
    for (_, r) in &results {
        if let Err((msg, span)) = r {
            if scan_err.as_ref().is_none_or(|(_, s)| span < s) {
                scan_err = Some((msg.clone(), *span));
            }
        }
    }
    if let Some((msg, span_start)) = scan_err {
        return Err(CompileError { msg, span_start });
    }

    // hollow-file resolve covers the rest statements; worker capture lists
    // supply what the blanked bodies hid
    let (mut captured, mutated, mut fn_caps) = resolve::Resolver::run(&program);
    for (key, caps) in results {
        let caps = caps.expect("scan errors returned above");
        for name in &caps {
            if let Some((_, id)) = name_ids.iter().find(|(n, _)| n == name) {
                captured.insert(*id);
            }
        }
        if !caps.is_empty() {
            fn_caps.insert(key, caps);
        }
    }
    // NOTE: emit receives the ORIGINAL source — lazy fills re-parse real
    // bodies; the hollow AST only drives main emission and signatures
    let out = emit::Emitter::compile(&program, captured, mutated, fn_caps, source, source_name);
    if phases {
        eprintln!(
            "[compile] parallel: hollow-parse={:?} workers={:?} resolve+emit={:?}",
            t1 - t0,
            t2 - t1,
            t2.elapsed()
        );
    }
    out
}

pub fn compile(source: &str, source_name: &str) -> Result<Chunk, CompileError> {
    let phases = std::env::var_os("TSC_COMPILE_PHASES").is_some();
    if emit::lazy_enabled() && std::env::var_os("TSC_NO_PARALLEL_FRONTEND").is_none() {
        if let Some(items) = split::split_top_level(source) {
            // worth the thread fan-out only for real bundles
            if items.len() >= 16 {
                return compile_parallel(source, source_name, &items, phases);
            }
        }
    }
    let t0 = std::time::Instant::now();
    // pre-size the AST arena: ~8x source is oxc's typical footprint, and
    // growth chunks mid-parse cost mmap + zeroing syscalls
    let allocator = Allocator::with_capacity(source.len() * 8);
    let program = tsc_ast::parse(&allocator, source).map_err(|errs| {
        let e = &errs[0];
        CompileError { msg: format!("parse error: {}", e.msg), span_start: e.span_start }
    })?;
    if let Some((msg, span_start)) = scan::subset_scan(&program.body) {
        return Err(CompileError { msg, span_start });
    }
    let t1 = std::time::Instant::now();
    let (captured, mutated, fn_caps) = resolve::Resolver::run(&program);
    let t2 = std::time::Instant::now();
    let out = emit::Emitter::compile(&program, captured, mutated, fn_caps, source, source_name);
    if phases {
        eprintln!(
            "[compile] parse={:?} resolve={:?} emit={:?}",
            t1 - t0,
            t2 - t1,
            t2.elapsed()
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_hello() {
        let chunk = compile(r#"console.log("hi");"#, "test.ts").unwrap_or_else(|e| panic!("{}", e.msg));
        assert!(!chunk.main.body().code.is_empty());
    }

    #[test]
    fn strips_types() {
        if let Err(e) = compile(
            "interface P { x: number }\ntype Q = string;\nconst a: number = 1;",
            "t.ts",
        ) {
            panic!("{}", e.msg);
        }
    }

    #[test]
    fn rejects_class() {
        match compile("class A {}", "t.ts") {
            Err(err) => assert!(err.msg.contains("not supported in M1"), "{}", err.msg),
            Ok(_) => panic!("class should be rejected"),
        }
    }
}
