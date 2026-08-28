//! tsc-ast
//!
//! Single point of OXC coupling: re-exports the pinned oxc crates so the
//! rest of the compiler imports through here. An OXC upgrade touches this
//! crate (and tsc-parser's walkers) only.

pub use oxc_allocator;
pub use oxc_ast;
pub use oxc_parser;
pub use oxc_span;
pub use oxc_syntax;

use oxc_allocator::Allocator;
use oxc_ast::ast::Program;
use oxc_span::SourceType;

pub struct ParseError {
    pub msg: String,
    pub span_start: u32,
}

/// Parse TypeScript source. Type annotations survive in the AST and are
/// ignored (stripped) by the bytecode emitter.
pub fn parse<'a>(
    allocator: &'a Allocator,
    source: &'a str,
) -> Result<Program<'a>, Vec<ParseError>> {
    let ret = oxc_parser::Parser::new(allocator, source, SourceType::ts()).parse();
    if ret.errors.is_empty() {
        Ok(ret.program)
    } else {
        Err(ret
            .errors
            .into_iter()
            .map(|e| ParseError {
                span_start: e
                    .labels
                    .as_ref()
                    .and_then(|l| l.first())
                    .map(|l| l.offset() as u32)
                    .unwrap_or(0),
                msg: e.message.to_string(),
            })
            .collect())
    }
}
