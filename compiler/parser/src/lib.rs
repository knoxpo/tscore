//! tsc-parser
//!
//! TypeScript → bytecode: OXC parse (types stripped), capture analysis,
//! single-pass emission. The M1 subset gate lives in the emitter — anything
//! outside TS-M1 fails here with a source span, never at runtime.

mod emit;
mod resolve;

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

pub fn compile(source: &str, source_name: &str) -> Result<Chunk, CompileError> {
    let phases = std::env::var_os("TSC_COMPILE_PHASES").is_some();
    let t0 = std::time::Instant::now();
    let allocator = Allocator::default();
    let program = tsc_ast::parse(&allocator, source).map_err(|errs| {
        let e = &errs[0];
        CompileError { msg: format!("parse error: {}", e.msg), span_start: e.span_start }
    })?;
    let t1 = std::time::Instant::now();
    let (captured, mutated) = resolve::Resolver::run(&program);
    let t2 = std::time::Instant::now();
    let out = emit::Emitter::compile(&program, captured, mutated, source_name);
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
        assert!(!chunk.main.code.is_empty());
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
