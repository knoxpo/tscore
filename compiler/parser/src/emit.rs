//! Single-pass bytecode emitter over the TS-M1 subset.
//! Stack-discipline register allocation: locals hold fixed low registers,
//! temporaries are allocated above and freed after each expression.

use crate::resolve::{BindingId, FnId};
use crate::CompileError;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tsc_ast::oxc_ast::ast::*;
use tsc_ast::oxc_span::GetSpan;
use tsc_ast::oxc_syntax::operator::{
    AssignmentOperator, BinaryOperator, LogicalOperator, UnaryOperator, UpdateOperator,
};
use tsc_ir::{Const, FunctionProto, Instr, Op, UpvalSrc};

#[derive(Clone, Copy)]
struct Local {
    reg: u8,
    /// Captured AND mutated: lives in a heap cell. Captured-immutable
    /// bindings stay plain registers (closures copy the value).
    cell: bool,
    /// Captured at all (cell or by-value).
    captured: bool,
}

#[derive(Clone, Copy)]
enum Place {
    Local(Local),
    Upval(u8),
    Global,
}

#[derive(PartialEq, Eq, Hash)]
enum ConstKey {
    Num(u64),
    Str(String),
}

struct LoopCtx {
    break_jumps: Vec<usize>,
    continue_jumps: Vec<usize>,
}

#[derive(Default)]
struct FuncState {
    name: String,
    arity: u8,
    is_async: bool,
    arg_types: Vec<tsc_ir::TypeHint>,
    code: Vec<Instr>,
    spans: Vec<u32>,
    consts: Vec<Const>,
    const_map: HashMap<ConstKey, u16>,
    protos: Vec<Arc<FunctionProto>>,
    upvals: Vec<(String, UpvalSrc)>,
    scopes: Vec<HashMap<String, Local>>,
    next_reg: u8,
    max_reg: u8,
    loops: Vec<LoopCtx>,
}

pub struct Emitter {
    fs: Vec<FuncState>,
    captured: HashSet<BindingId>,
    mutated: HashSet<BindingId>,
    /// Per-function ordered capture-name lists from the resolver (M6b):
    /// lets a child's upvalue table be built before its body is emitted.
    fn_caps: HashMap<FnId, Vec<String>>,
    /// Retained source for lazy body fills (M6c); None inside a fill.
    source: Option<Arc<str>>,
    /// Originating file, stamped on every created proto (error attribution
    /// across module files).
    src_name: Arc<str>,
    /// Module compile context (imports/exports lowering); None = script.
    module: Option<crate::ModuleCtx>,
    /// Defer direct children of <main> (M6c); off inside fills and under
    /// TSC_NO_LAZY=1.
    lazy: bool,
    cur_span: u32,
    /// Set by an expression statement for the one `expr` call that
    /// lowers its top-level expression: the value is discarded, so a
    /// lowering with an optional result (push's returned length) may
    /// skip producing it. Taken at `expr` entry, so nested calls never
    /// see it.
    discard_next: bool,
    /// The value `expr` took for the expression it is currently lowering;
    /// a lowering reads it before it lowers any sub-expression.
    discard_cur: bool,
}

/// Everything a deferred body fill needs (payload of ir::LazySource).
struct LazyPayload {
    source: Arc<str>,
    src_name: Arc<str>,
    module: Option<crate::ModuleCtx>,
    start: u32,
    end: u32,
    /// Expression forms (arrows, function expressions) re-parse wrapped in
    /// parentheses; declarations parse bare.
    wrap: bool,
    name: String,
    is_async: bool,
    /// Upvalue names in the proto's stored order — GetUpval indices in the
    /// filled body must match the table built at closure-creation time.
    env: Vec<String>,
}

pub(crate) fn lazy_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("TSC_NO_LAZY").is_none())
}

/// Compile a deferred function body (M6c). The snippet is left-padded
/// with spaces to its original file offset, so every span — error
/// positions, binding ids, capture-table keys — matches the original
/// source with no remapping.
/// ponytail: padding costs O(offset) bytes per fill; slice-relative spans
/// with an offset field if profiles ever mind.
fn fill_lazy(lz: &tsc_ir::LazySource) -> Result<tsc_ir::ProtoBody, (String, u32)> {
    let p: &LazyPayload = lz
        .payload
        .downcast_ref()
        .expect("lazy payload type");
    let start = p.start as usize;
    let end = p.end as usize;
    let mut text = String::with_capacity(end + 1);
    if p.wrap && start > 0 {
        text.push_str(&" ".repeat(start - 1));
        text.push('(');
    } else {
        text.push_str(&" ".repeat(start));
    }
    text.push_str(&p.source[start..end]);
    if p.wrap {
        text.push(')');
    }
    let allocator = tsc_ast::oxc_allocator::Allocator::default();
    let program = tsc_ast::parse(&allocator, &text)
        .map_err(|errs| (format!("parse error: {}", errs[0].msg), errs[0].span_start))?;
    let (captured, mutated, fn_caps) = crate::resolve::Resolver::run_seeded(&program, &p.env);

    let mut em = Emitter {
        fs: Vec::new(),
        captured,
        mutated,
        fn_caps,
        source: None,
        src_name: p.src_name.clone(),
        module: p.module.clone(),
        lazy: false,
        cur_span: p.start,
        discard_next: false,
        discard_cur: false,
    };
    let mut fs = FuncState {
        name: p.name.clone(),
        scopes: vec![HashMap::new()],
        is_async: p.is_async,
        ..Default::default()
    };
    // stored env order is canonical: GetUpval indices in the parent-built
    // table must match (srcs are unused during body emission)
    for n in &p.env {
        fs.upvals.push((n.clone(), UpvalSrc::ParentUpval(0)));
    }
    em.fs.push(fs);

    // locate the function node and emit its body
    let internal =
        || ("internal: lazy snippet did not re-parse to a function".to_string(), p.start);
    let r = match program.body.first().ok_or_else(internal)? {
        Statement::FunctionDeclaration(f) => {
            let body = f.body.as_ref().ok_or_else(|| {
                ("not supported in M1: declare function".to_string(), p.start)
            })?;
            em.emit_function_body(&f.params, &body.statements, None)
        }
        Statement::ExpressionStatement(es) => {
            let mut e = &es.expression;
            while let Expression::ParenthesizedExpression(pe) = e {
                e = &pe.expression;
            }
            match e {
                Expression::ArrowFunctionExpression(a) => {
                    let expr_body = if a.expression {
                        match a.body.statements.first() {
                            Some(Statement::ExpressionStatement(e)) => Some(&e.expression),
                            _ => None,
                        }
                    } else {
                        None
                    };
                    em.emit_function_body(&a.params, &a.body.statements, expr_body)
                }
                Expression::FunctionExpression(f) => {
                    let body = f.body.as_ref().ok_or_else(|| {
                        ("not supported in M1: declare function".to_string(), p.start)
                    })?;
                    em.emit_function_body(&f.params, &body.statements, None)
                }
                _ => return Err(internal()),
            }
        }
        _ => return Err(internal()),
    };
    r.map_err(|e| (e.msg, e.span_start))?;
    let fs = em.fs.pop().unwrap();
    Ok(finish_body(fs))
}

type R<T = ()> = Result<T, CompileError>;

impl Emitter {
    pub fn compile(
        program: &Program,
        captured: HashSet<BindingId>,
        mutated: HashSet<BindingId>,
        fn_caps: HashMap<FnId, Vec<String>>,
        source: &str,
        source_name: &str,
    ) -> R<tsc_ir::Chunk> {
        Self::compile_with(program, captured, mutated, fn_caps, source, source_name, None)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn compile_with(
        program: &Program,
        captured: HashSet<BindingId>,
        mutated: HashSet<BindingId>,
        fn_caps: HashMap<FnId, Vec<String>>,
        source: &str,
        source_name: &str,
        module: Option<crate::ModuleCtx>,
    ) -> R<tsc_ir::Chunk> {
        let mut em = Emitter {
            fs: Vec::new(),
            captured,
            mutated,
            fn_caps,
            source: Some(Arc::from(source)),
            src_name: Arc::from(source_name),
            module,
            lazy: lazy_enabled(),
            cur_span: 0,
            discard_next: false,
        discard_cur: false,
    };
        em.fs.push(FuncState {
            name: "<main>".into(),
            scopes: vec![HashMap::new()],
            is_async: true, // allows top-level await; downgraded after emit if unused
            ..Default::default()
        });
        em.hoist_functions(&program.body)?;
        for s in &program.body {
            em.stmt(s)?;
        }
        em.emit(Op::Halt, 0, 0, 0);
        let mut fs = em.fs.pop().unwrap();
        // Only stay async if top-level await was actually used: a sync
        // <main> is eligible for OSR/Tier-2 (async frames never JIT).
        fs.is_async = fs.code.iter().any(|i| i.op == Op::Await);
        Ok(tsc_ir::Chunk {
            main: {
                let m = Arc::new(finish(fs));
                m.set_source_name(&em.src_name);
                m
            },
            source_name: source_name.into(),
        })
    }

    fn unsupported<T>(&self, what: &str, span: u32) -> R<T> {
        Err(CompileError {
            msg: format!("not supported in M1: {what}"),
            span_start: span,
        })
    }

    fn f(&mut self) -> &mut FuncState {
        self.fs.last_mut().unwrap()
    }

    fn emit(&mut self, op: Op, a: u8, b: u8, c: u8) -> usize {
        let span = self.cur_span;
        let f = self.f();
        f.code.push(Instr::abc(op, a, b, c));
        f.spans.push(span);
        f.code.len() - 1
    }

    fn emit_abx(&mut self, op: Op, a: u8, bx: u16) -> usize {
        let span = self.cur_span;
        let f = self.f();
        f.code.push(Instr::abx(op, a, bx));
        f.spans.push(span);
        f.code.len() - 1
    }

    fn emit_jump(&mut self, op: Op, a: u8) -> usize {
        self.emit_abx(op, a, 0)
    }

    /// Patch jump at `at` to land on the next emitted instruction.
    fn patch_here(&mut self, at: usize) {
        let here = self.f().code.len();
        let sbx = here as i32 - at as i32 - 1;
        let ins = self.f().code[at];
        self.f().code[at] = Instr::asbx(ins.op, ins.a, sbx);
    }

    fn jump_back(&mut self, op: Op, a: u8, target: usize) {
        let at = self.f().code.len();
        let sbx = target as i32 - at as i32 - 1;
        let span = self.cur_span;
        let f = self.f();
        f.code.push(Instr::asbx(op, a, sbx));
        f.spans.push(span);
    }

    fn konst(&mut self, c: Const) -> u16 {
        let key = match &c {
            Const::Number(n) => ConstKey::Num(n.to_bits()),
            Const::Str(s) => ConstKey::Str(s.to_string()),
            Const::Keys(ks) => ConstKey::Str(format!("\0keys:{}", ks.join("\0"))),
        };
        let f = self.f();
        if let Some(&i) = f.const_map.get(&key) {
            return i;
        }
        f.consts.push(c);
        let i = (f.consts.len() - 1) as u16;
        f.const_map.insert(key, i);
        i
    }

    fn str_const(&mut self, s: &str) -> u16 {
        self.konst(Const::Str(Arc::from(s)))
    }

    fn alloc_reg(&mut self, span: u32) -> R<u8> {
        let f = self.f();
        if f.next_reg == u8::MAX {
            return Err(CompileError {
                msg: "function too complex: out of registers".into(),
                span_start: span,
            });
        }
        let r = f.next_reg;
        f.next_reg += 1;
        f.max_reg = f.max_reg.max(f.next_reg);
        Ok(r)
    }

    fn mark(&mut self) -> u8 {
        self.fs.last().unwrap().next_reg
    }

    fn free_to(&mut self, mark: u8) {
        self.f().next_reg = mark;
    }

    /// (export name, mutable) when `name` is a top-level module export and
    /// we are emitting <main>'s top scope.
    fn export_info(&self, name: &str) -> Option<(Arc<str>, bool)> {
        if self.fs.len() != 1 || self.fs[0].scopes.len() != 1 {
            return None;
        }
        self.module.as_ref()?.exports.get(name).cloned()
    }

    /// Publish an export into the namespace: the cell itself for mutable
    /// bindings (readers deref through GetField — live), the value for
    /// immutable ones.
    fn export_value_if_needed(&mut self, name: &str, local: Local, tmp: u8) -> R {
        if let Some((export_name, _)) = self.export_info(name) {
            let key = self.module.as_ref().unwrap().key.clone();
            let mark = self.mark();
            let ns = self.alloc_reg(self.cur_span)?;
            let k = self.str_const(&key);
            self.emit_abx(Op::GetGlobal, ns, k);
            let f = self.str_const(&export_name);
            let src = if local.cell { local.reg } else if local.reg != tmp { local.reg } else { tmp };
            // SetField A[const B] = C
            self.emit(Op::SetField, ns, f as u8, src);
            self.free_to(mark);
        }
        Ok(())
    }

    /// Initialize a cell-local. Module exports publish the cell into the
    /// namespace afterwards (export_value_if_needed) — importers read the
    /// field and GetField auto-derefs cells, giving live bindings.
    fn cell_init(&mut self, local: Local, _name: &str) {
        self.emit(Op::NewCell, local.reg, 0, 0);
    }

    fn declare_local(&mut self, name: &str, binding_span: u32) -> R<Local> {
        let captured = self.captured.contains(&binding_span);
        let mut cell = captured && self.mutated.contains(&binding_span);
        // a mutated module export always lives in a (host-provided) cell,
        // captured or not — importers observe reassignments through it
        if self.mutated.contains(&binding_span) && self.export_info(name).map_or(false, |e| e.1) {
            cell = true;
        }
        let reg = self.alloc_reg(binding_span)?;
        let local = Local { reg, cell, captured };
        self.f().scopes.last_mut().unwrap().insert(name.to_string(), local);
        Ok(local)
    }

    /// Resolve a name from the innermost function outward, threading
    /// upvalue chains through intermediate closures.
    fn resolve(&mut self, name: &str) -> Place {
        fn find_local(fs: &FuncState, name: &str) -> Option<Local> {
            fs.scopes.iter().rev().find_map(|s| s.get(name).copied())
        }
        fn find_upval(fs: &FuncState, name: &str) -> Option<u8> {
            fs.upvals.iter().position(|(n, _)| n == name).map(|i| i as u8)
        }
        let top = self.fs.len() - 1;
        if let Some(l) = find_local(&self.fs[top], name) {
            return Place::Local(l);
        }
        if let Some(i) = find_upval(&self.fs[top], name) {
            return Place::Upval(i);
        }
        // find the level that owns the binding
        let mut owner = None;
        for i in (0..top).rev() {
            if find_local(&self.fs[i], name).is_some() || find_upval(&self.fs[i], name).is_some() {
                owner = Some(i);
                break;
            }
        }
        let Some(owner) = owner else { return Place::Global };
        // thread the capture from owner+1 up to top
        for level in owner + 1..=top {
            if find_upval(&self.fs[level], name).is_some() {
                continue;
            }
            let src = if let Some(l) = find_local(&self.fs[level - 1], name) {
                debug_assert!(l.captured, "resolver missed a capture");
                if l.cell {
                    UpvalSrc::ParentLocal(l.reg)
                } else {
                    UpvalSrc::ParentLocalValue(l.reg)
                }
            } else {
                let i = find_upval(&self.fs[level - 1], name)
                    .expect("upvalue chain broken");
                UpvalSrc::ParentUpval(i)
            };
            if std::env::var_os("TSC_CAPS_DEBUG").is_some() {
                eprintln!("[caps] threading fallback used for '{name}'");
            }
            self.fs[level].upvals.push((name.to_string(), src));
        }
        Place::Upval(find_upval(&self.fs[top], name).unwrap())
    }

    fn load_place(&mut self, place: Place, name: &str, dst: u8) {
        match place {
            Place::Local(l) if l.cell => {
                self.emit(Op::LoadCell, dst, l.reg, 0);
            }
            Place::Local(l) => {
                if l.reg != dst {
                    self.emit(Op::Move, dst, l.reg, 0);
                }
            }
            Place::Upval(i) => {
                self.emit(Op::GetUpval, dst, i, 0);
            }
            Place::Global => {
                if let Some(b) = self
                    .module
                    .as_ref()
                    .and_then(|m| m.imports.get(name))
                    .cloned()
                {
                    let k = self.str_const(&b.dep_key);
                    self.emit_abx(Op::GetGlobal, dst, k);
                    match b.kind {
                        crate::ImportKind::Namespace(_) => {}
                        crate::ImportKind::Value(ref n) => {
                            let f = self.str_const(n);
                            self.emit(Op::GetField, dst, dst, f as u8);
                        }
                        crate::ImportKind::Cell(ref n) => {
                            // GetField auto-derefs ns cells (helper path)
                            let f = self.str_const(n);
                            self.emit(Op::GetField, dst, dst, f as u8);
                        }
                    }
                    return;
                }
                let k = self.str_const(name);
                self.emit_abx(Op::GetGlobal, dst, k);
            }
        }
    }

    fn store_place(&mut self, place: Place, name: &str, src: u8, span: u32) -> R {
        match place {
            Place::Local(l) if l.cell => {
                self.emit(Op::StoreCell, l.reg, src, 0);
            }
            Place::Local(l) => {
                if l.reg != src {
                    self.emit(Op::Move, l.reg, src, 0);
                }
            }
            Place::Upval(i) => {
                self.emit(Op::SetUpval, i, src, 0);
            }
            Place::Global => {
                if self
                    .module
                    .as_ref()
                    .is_some_and(|m| m.imports.contains_key(name))
                {
                    return self.unsupported(
                        &format!("assignment to import '{name}'"),
                        span,
                    );
                }
                return self.unsupported(&format!("assignment to global '{name}'"), span)
            }
        }
        Ok(())
    }

    /// Register holding a plain (non-captured) local — usable directly as
    /// an operand, skipping a Move to a temp.
    fn local_operand(&mut self, e: &Expression) -> Option<u8> {
        if let Expression::Identifier(id) = e {
            if !matches!(&*id.name, "undefined" | "NaN" | "Infinity") {
                if let Place::Local(l) = self.resolve(&id.name) {
                    if !l.cell {
                        return Some(l.reg);
                    }
                }
            }
        }
        None
    }

    /// True when evaluating `e` cannot write any local (safe to read the
    /// left operand's register after evaluating `e`).
    fn is_effect_free(e: &Expression) -> bool {
        match e {
            Expression::NumericLiteral(_)
            | Expression::BooleanLiteral(_)
            | Expression::StringLiteral(_)
            | Expression::NullLiteral(_)
            | Expression::Identifier(_) => true,
            Expression::ParenthesizedExpression(p) => Self::is_effect_free(&p.expression),
            Expression::StaticMemberExpression(m) => Self::is_effect_free(&m.object),
            Expression::ComputedMemberExpression(m) => {
                Self::is_effect_free(&m.object) && Self::is_effect_free(&m.expression)
            }
            Expression::UnaryExpression(u) => Self::is_effect_free(&u.argument),
            Expression::BinaryExpression(b) => {
                Self::is_effect_free(&b.left) && Self::is_effect_free(&b.right)
            }
            _ => false,
        }
    }

    /// Emit a loop/if condition and return the jump-to-patch for the false
    /// path. A test that is syntactically a plain comparison fuses into a
    /// compare-and-skip op (one dispatch on the hot path). Only whole-test
    /// comparisons fuse — short-circuit tests keep the two-op form because
    /// their internal jumps may land right after the compare.
    fn emit_test_jump(&mut self, test: &Expression) -> R<usize> {
        let mut t = test;
        while let Expression::ParenthesizedExpression(p) = t {
            t = &p.expression;
        }
        if let Expression::BinaryExpression(b) = t {
            let skip_op = match b.operator {
                BinaryOperator::Equality | BinaryOperator::StrictEquality => Some(Op::EqSkip),
                BinaryOperator::Inequality | BinaryOperator::StrictInequality => {
                    Some(Op::NeSkip)
                }
                BinaryOperator::LessThan => Some(Op::LtSkip),
                BinaryOperator::LessEqualThan => Some(Op::LeSkip),
                BinaryOperator::GreaterThan => Some(Op::GtSkip),
                BinaryOperator::GreaterEqualThan => Some(Op::GeSkip),
                _ => None,
            };
            if let Some(op) = skip_op {
                let mark = self.mark();
                let rb = match self.local_operand(&b.left) {
                    Some(r) if Self::is_effect_free(&b.right) => r,
                    _ => {
                        let r = self.alloc_reg(b.span.start)?;
                        self.expr(&b.left, r)?;
                        r
                    }
                };
                let rc = match self.local_operand(&b.right) {
                    Some(r) => r,
                    None => {
                        let r = self.alloc_reg(b.span.start)?;
                        self.expr(&b.right, r)?;
                        r
                    }
                };
                self.cur_span = b.span.start;
                self.emit(op, 0, rb, rc);
                let j = self.emit_jump(Op::Jump, 0);
                self.free_to(mark);
                return Ok(j);
            }
        }
        let mark = self.mark();
        let cond = self.alloc_reg(test.span().start)?;
        self.expr(test, cond)?;
        let j = self.emit_jump(Op::JumpIfFalse, cond);
        self.free_to(mark);
        Ok(j)
    }

    fn hoist_functions(&mut self, stmts: &[Statement]) -> R {
        for s in stmts {
            let f = match s {
                Statement::FunctionDeclaration(f) => Some(f),
                Statement::ExportNamedDeclaration(e) => match &e.declaration {
                    Some(tsc_ast::oxc_ast::ast::Declaration::FunctionDeclaration(f)) => Some(f),
                    _ => None,
                },
                Statement::ExportDefaultDeclaration(e) => match &e.declaration {
                    tsc_ast::oxc_ast::ast::ExportDefaultDeclarationKind::FunctionDeclaration(f) => {
                        Some(f)
                    }
                    _ => None,
                },
                _ => None,
            };
            if let Some(f) = f {
                if let Some(id) = &f.id {
                    let local = self.declare_local(&id.name, id.span.start)?;
                    self.emit(Op::LoadUndef, local.reg, 0, 0);
                    if local.cell {
                        self.cell_init(local, &id.name);
                    }
                }
            }
        }
        Ok(())
    }

    // ---------------- statements ----------------

    fn stmt(&mut self, s: &Statement) -> R {
        self.cur_span = s.span().start;
        match s {
            Statement::VariableDeclaration(d) => {
                for decl in &d.declarations {
                    let BindingPattern::BindingIdentifier(b) = &decl.id else {
                        return self.unsupported(
                            "destructuring declarations",
                            decl.span.start,
                        );
                    };
                    // init evaluates before the binding exists (TDZ-ish)
                    let mark = self.mark();
                    let tmp = self.alloc_reg(decl.span.start)?;
                    match &decl.init {
                        Some(init) => self.expr(init, tmp)?,
                        None => {
                            self.emit(Op::LoadUndef, tmp, 0, 0);
                        }
                    }
                    // cell locals must keep tmp alive past declare (the
                    // freed-tmp/local alias would store the cell into
                    // itself); plain locals keep the alias optimization
                    let will_cell = {
                        let cap = self.captured.contains(&b.span.start);
                        let mu = self.mutated.contains(&b.span.start);
                        (cap && mu)
                            || (mu && self.export_info(&b.name).map_or(false, |e| e.1))
                    };
                    if !will_cell {
                        self.free_to(mark);
                    }
                    let local = self.declare_local(&b.name, b.span.start)?;
                    if local.cell {
                        self.cell_init(local, &b.name);
                        self.emit(Op::StoreCell, local.reg, tmp, 0);
                        // tmp leaks one register (freeing to mark would
                        // release the local's own reg allocated after it)
                    } else if local.reg != tmp {
                        self.emit(Op::Move, local.reg, tmp, 0);
                    }
                    self.export_value_if_needed(&b.name, local, tmp)?;
                }
                Ok(())
            }
            Statement::FunctionDeclaration(f) => {
                let Some(id) = &f.id else {
                    return self.unsupported("anonymous function declaration", f.span.start);
                };
                let Some(body) = &f.body else {
                    return self.unsupported("declare function", f.span.start);
                };
                let mark = self.mark();
                let tmp = self.alloc_reg(f.span.start)?;
                let proto_idx = self.compile_function(
                    f.span.start,
                    &id.name,
                    &f.params,
                    &body.statements,
                    f.r#async,
                    Some((f.span.start, f.span.end, false)),
                )?;
                self.emit_abx(Op::Closure, tmp, proto_idx);
                let place = self.resolve(&id.name);
                self.store_place(place, &id.name, tmp, id.span.start)?;
                self.free_to(mark);
                Ok(())
            }
            Statement::ExpressionStatement(e) => {
                let mark = self.mark();
                let tmp = self.alloc_reg(e.span.start)?;
                self.discard_next = true;
                let r = self.expr(&e.expression, tmp);
                self.discard_next = false;
                r?;
                self.free_to(mark);
                Ok(())
            }
            Statement::ImportDeclaration(i) => {
                if self.module.is_none() {
                    return self.unsupported("import (module pipeline only)", i.span.start);
                }
                Ok(()) // bindings resolve through the import map
            }
            Statement::ExportAllDeclaration(e) => {
                if self.module.is_none() {
                    return self.unsupported("export (module pipeline only)", e.span.start);
                }
                Ok(()) // the host materializes star re-exports
            }
            Statement::ExportNamedDeclaration(e) => {
                if self.module.is_none() {
                    return self.unsupported("export (module pipeline only)", e.span.start);
                }
                if e.export_kind.is_type() {
                    return Ok(());
                }
                if e.source.is_some() {
                    return Ok(()); // re-exports are materialized by the host
                }
                if let Some(d) = &e.declaration {
                    use tsc_ast::oxc_ast::ast::Declaration as D;
                    match d {
                        D::VariableDeclaration(v) => {
                            for decl in &v.declarations {
                                let BindingPattern::BindingIdentifier(b) = &decl.id else {
                                    return self.unsupported(
                                        "destructuring declarations",
                                        decl.span.start,
                                    );
                                };
                                let mark = self.mark();
                                let tmp = self.alloc_reg(decl.span.start)?;
                                match &decl.init {
                                    Some(init) => self.expr(init, tmp)?,
                                    None => {
                                        self.emit(Op::LoadUndef, tmp, 0, 0);
                                    }
                                }
                                let will_cell = {
                                    let cap = self.captured.contains(&b.span.start);
                                    let mu = self.mutated.contains(&b.span.start);
                                    (cap && mu)
                                        || (mu
                                            && self
                                                .export_info(&b.name)
                                                .map_or(false, |e| e.1))
                                };
                                if !will_cell {
                                    self.free_to(mark);
                                }
                                let local = self.declare_local(&b.name, b.span.start)?;
                                if local.cell {
                                    self.cell_init(local, &b.name);
                                    self.emit(Op::StoreCell, local.reg, tmp, 0);
                                } else if local.reg != tmp {
                                    self.emit(Op::Move, local.reg, tmp, 0);
                                }
                                self.export_value_if_needed(&b.name, local, tmp)?;
                            }
                        }
                        D::FunctionDeclaration(f) => {
                            let Some(id) = &f.id else {
                                return self.unsupported(
                                    "anonymous function declaration",
                                    f.span.start,
                                );
                            };
                            let Some(body) = &f.body else {
                                return self.unsupported("declare function", f.span.start);
                            };
                            let mark = self.mark();
                            let tmp = self.alloc_reg(f.span.start)?;
                            let proto_idx = self.compile_function(
                                f.span.start,
                                &id.name,
                                &f.params,
                                &body.statements,
                                f.r#async,
                                Some((f.span.start, f.span.end, false)),
                            )?;
                            self.emit_abx(Op::Closure, tmp, proto_idx);
                            let place = self.resolve(&id.name);
                            self.store_place(place, &id.name, tmp, id.span.start)?;
                            if let Place::Local(local) = place {
                                self.export_value_if_needed(&id.name, local, tmp)?;
                            }
                            self.free_to(mark);
                        }
                        _ => return Ok(()), // type-only declarations
                    }
                    return Ok(());
                }
                // `export { a, b as c }`: mutable ones already share the ns
                // cell; immutable ones publish their current value
                for sp in &e.specifiers {
                    let local_name = sp.local.name().to_string();
                    let export_name: Arc<str> = Arc::from(sp.exported.name().as_str());
                    let mark = self.mark();
                    let tmp = self.alloc_reg(e.span.start)?;
                    let place = self.resolve(&local_name);
                    match place {
                        // mutable binding: publish the CELL, not a snapshot
                        Place::Local(l) if l.cell => {
                            self.emit(Op::Move, tmp, l.reg, 0);
                        }
                        _ => self.load_place(place, &local_name, tmp),
                    }
                    let ns = self.alloc_reg(e.span.start)?;
                    let key = self.module.as_ref().unwrap().key.clone();
                    let k = self.str_const(&key);
                    self.emit_abx(Op::GetGlobal, ns, k);
                    let f = self.str_const(&export_name);
                    self.emit(Op::SetField, ns, f as u8, tmp);
                    self.free_to(mark);
                }
                Ok(())
            }
            Statement::ExportDefaultDeclaration(e) => {
                if self.module.is_none() {
                    return self.unsupported("export (module pipeline only)", e.span.start);
                }
                use tsc_ast::oxc_ast::ast::ExportDefaultDeclarationKind as K;
                let mark = self.mark();
                let tmp = self.alloc_reg(e.span.start)?;
                match &e.declaration {
                    K::FunctionDeclaration(f) => {
                        let Some(body) = &f.body else {
                            return self.unsupported("declare function", f.span.start);
                        };
                        let name = f
                            .id
                            .as_ref()
                            .map(|i| i.name.to_string())
                            .unwrap_or_else(|| "<default>".into());
                        let proto_idx = self.compile_function(
                            f.span.start,
                            &name,
                            &f.params,
                            &body.statements,
                            f.r#async,
                            Some((f.span.start, f.span.end, false)),
                        )?;
                        self.emit_abx(Op::Closure, tmp, proto_idx);
                        // named default fn is also bound locally
                        if let Some(id) = &f.id {
                            let local = self.declare_local(&id.name, id.span.start)?;
                            if local.cell {
                                self.cell_init(local, &id.name);
                                self.emit(Op::StoreCell, local.reg, tmp, 0);
                            } else if local.reg != tmp {
                                self.emit(Op::Move, local.reg, tmp, 0);
                            }
                        }
                    }
                    other => {
                        let Some(expr) = other.as_expression() else {
                            return self.unsupported("export default form", e.span.start);
                        };
                        self.expr(expr, tmp)?;
                    }
                }
                let ns = self.alloc_reg(e.span.start)?;
                let key = self.module.as_ref().unwrap().key.clone();
                let k = self.str_const(&key);
                self.emit_abx(Op::GetGlobal, ns, k);
                let f = self.str_const("default");
                self.emit(Op::SetField, ns, f as u8, tmp);
                self.free_to(mark);
                Ok(())
            }
            Statement::ReturnStatement(r) => {
                if let Some(arg) = &r.argument {
                    if let Some(reg) = self.local_operand(arg) {
                        self.emit(Op::Return, reg, 0, 0);
                        return Ok(());
                    }
                }
                let mark = self.mark();
                let tmp = self.alloc_reg(r.span.start)?;
                match &r.argument {
                    Some(arg) => self.expr(arg, tmp)?,
                    None => {
                        self.emit(Op::LoadUndef, tmp, 0, 0);
                    }
                }
                self.emit(Op::Return, tmp, 0, 0);
                self.free_to(mark);
                Ok(())
            }
            Statement::IfStatement(i) => {
                let jf = self.emit_test_jump(&i.test)?;
                self.stmt(&i.consequent)?;
                match &i.alternate {
                    Some(alt) => {
                        let jend = self.emit_jump(Op::Jump, 0);
                        self.patch_here(jf);
                        self.stmt(alt)?;
                        self.patch_here(jend);
                    }
                    None => self.patch_here(jf),
                }
                Ok(())
            }
            Statement::WhileStatement(w) => {
                let top = self.f().code.len();
                let jf = self.emit_test_jump(&w.test)?;
                self.f().loops.push(LoopCtx {
                    break_jumps: vec![],
                    continue_jumps: vec![],
                });
                self.stmt(&w.body)?;
                let ctx = self.f().loops.pop().unwrap();
                for j in ctx.continue_jumps {
                    self.jump_target(j, top);
                }
                self.jump_back(Op::Jump, 0, top);
                self.patch_here(jf);
                for j in ctx.break_jumps {
                    self.patch_here(j);
                }
                Ok(())
            }
            Statement::ForStatement(f) => {
                self.f().scopes.push(HashMap::new());
                let outer_mark = self.mark();
                match &f.init {
                    Some(ForStatementInit::VariableDeclaration(d)) => {
                        // reuse VariableDeclaration handling
                        for decl in &d.declarations {
                            let BindingPattern::BindingIdentifier(b) = &decl.id
                            else {
                                return self
                                    .unsupported("destructuring", decl.span.start);
                            };
                            let mark = self.mark();
                            let tmp = self.alloc_reg(decl.span.start)?;
                            match &decl.init {
                                Some(init) => self.expr(init, tmp)?,
                                None => {
                                    self.emit(Op::LoadUndef, tmp, 0, 0);
                                }
                            }
                            self.free_to(mark);
                            let local = self.declare_local(&b.name, b.span.start)?;
                            if local.reg != tmp {
                                self.emit(Op::Move, local.reg, tmp, 0);
                            }
                            if local.cell {
                                self.emit(Op::NewCell, local.reg, 0, 0);
                            }
                        }
                    }
                    Some(init) => {
                        if let Some(e) = init.as_expression() {
                            let mark = self.mark();
                            let tmp = self.alloc_reg(f.span.start)?;
                            self.expr(e, tmp)?;
                            self.free_to(mark);
                        }
                    }
                    None => {}
                }
                let top = self.f().code.len();
                let jf = match &f.test {
                    Some(test) => Some(self.emit_test_jump(test)?),
                    None => None,
                };
                self.f().loops.push(LoopCtx {
                    break_jumps: vec![],
                    continue_jumps: vec![],
                });
                self.stmt(&f.body)?;
                let ctx = self.f().loops.pop().unwrap();
                let update_at = self.f().code.len();
                for j in ctx.continue_jumps {
                    self.jump_target(j, update_at);
                }
                if let Some(update) = &f.update {
                    let mark = self.mark();
                    let tmp = self.alloc_reg(f.span.start)?;
                    self.expr(update, tmp)?;
                    self.free_to(mark);
                }
                self.jump_back(Op::Jump, 0, top);
                if let Some(jf) = jf {
                    self.patch_here(jf);
                }
                for j in ctx.break_jumps {
                    self.patch_here(j);
                }
                self.f().scopes.pop();
                self.free_to(outer_mark);
                Ok(())
            }
            Statement::ForOfStatement(f) => {
                let ForStatementLeft::VariableDeclaration(d) = &f.left else {
                    return self.unsupported("for-of over non-declaration", f.span.start);
                };
                let BindingPattern::BindingIdentifier(b) =
                    &d.declarations[0].id
                else {
                    return self.unsupported("destructuring in for-of", f.span.start);
                };
                self.f().scopes.push(HashMap::new());
                let outer_mark = self.mark();
                let arr = self.alloc_reg(f.span.start)?;
                self.expr(&f.right, arr)?;
                let idx = self.alloc_reg(f.span.start)?;
                let len = self.alloc_reg(f.span.start)?;
                self.emit_abx(Op::LoadInt, idx, tsc_ir::SBX_BIAS as u16); // 0
                let elem = self.declare_local(&b.name, b.span.start)?;
                let top = self.f().code.len();
                self.emit(Op::Len, len, arr, 0);
                self.emit(Op::LtSkip, 0, idx, len);
                let jf = self.emit_jump(Op::Jump, 0);
                self.emit(Op::GetIndex, elem.reg, arr, idx);
                if elem.cell {
                    // fresh cell per iteration = correct `let` semantics
                    self.emit(Op::NewCell, elem.reg, 0, 0);
                }
                self.f().loops.push(LoopCtx {
                    break_jumps: vec![],
                    continue_jumps: vec![],
                });
                self.stmt(&f.body)?;
                let ctx = self.f().loops.pop().unwrap();
                let inc_at = self.f().code.len();
                for j in ctx.continue_jumps {
                    self.jump_target(j, inc_at);
                }
                let one = self.alloc_reg(f.span.start)?;
                self.emit_abx(Op::LoadInt, one, (tsc_ir::SBX_BIAS + 1) as u16);
                self.emit(Op::Add, idx, idx, one);
                self.free_to(one);
                self.jump_back(Op::Jump, 0, top);
                self.patch_here(jf);
                for j in ctx.break_jumps {
                    self.patch_here(j);
                }
                self.f().scopes.pop();
                self.free_to(outer_mark);
                Ok(())
            }
            Statement::BreakStatement(b) => {
                if b.label.is_some() {
                    return self.unsupported("labeled break", b.span.start);
                }
                let j = self.emit_jump(Op::Jump, 0);
                match self.f().loops.last_mut() {
                    Some(ctx) => ctx.break_jumps.push(j),
                    None => return self.unsupported("break outside loop", b.span.start),
                }
                Ok(())
            }
            Statement::ContinueStatement(c) => {
                if c.label.is_some() {
                    return self.unsupported("labeled continue", c.span.start);
                }
                let j = self.emit_jump(Op::Jump, 0);
                match self.f().loops.last_mut() {
                    Some(ctx) => ctx.continue_jumps.push(j),
                    None => {
                        return self.unsupported("continue outside loop", c.span.start)
                    }
                }
                Ok(())
            }
            Statement::BlockStatement(b) => {
                self.f().scopes.push(HashMap::new());
                let mark = self.mark();
                self.hoist_functions(&b.body)?;
                for s in &b.body {
                    self.stmt(s)?;
                }
                self.f().scopes.pop();
                self.free_to(mark);
                Ok(())
            }
            Statement::EmptyStatement(_) => Ok(()),
            // type-only statements: parsed and stripped
            Statement::TSInterfaceDeclaration(_) | Statement::TSTypeAliasDeclaration(_) => {
                Ok(())
            }
            other => self.unsupported(
                &format!("statement {:?}", stmt_kind(other)),
                other.span().start,
            ),
        }
    }

    fn jump_target(&mut self, at: usize, target: usize) {
        let sbx = target as i32 - at as i32 - 1;
        let ins = self.f().code[at];
        self.f().code[at] = Instr::asbx(ins.op, ins.a, sbx);
    }

    // ---------------- functions ----------------

    #[allow(clippy::too_many_arguments)]
    fn compile_function(
        &mut self,
        key: FnId,
        name: &str,
        params: &FormalParameters,
        body: &[Statement],
        is_async: bool,
        lazy_span: Option<(u32, u32, bool)>,
    ) -> R<u16> {
        self.compile_function_inner(key, name, params, body, None, is_async, lazy_span)
    }

    #[allow(clippy::too_many_arguments)]
    fn compile_function_inner(
        &mut self,
        key: FnId,
        name: &str,
        params: &FormalParameters,
        body: &[Statement],
        expr_body: Option<&Expression>,
        is_async: bool,
        lazy_span: Option<(u32, u32, bool)>,
    ) -> R<u16> {
        let mut fs = FuncState {
            name: name.to_string(),
            scopes: vec![HashMap::new()],
            is_async,
            ..Default::default()
        };
        // M6b: build the upvalue table from the resolver's capture list
        // BEFORE the body is emitted — the lazy-compile prerequisite. The
        // parent's own table was pre-populated the same way, so chained
        // captures resolve without threading through live emission.
        if let Some(caps) = self.fn_caps.get(&key) {
            let parent = self.fs.last().unwrap();
            let caps = caps.clone();
            for cap in caps {
                let src = if let Some(l) =
                    parent.scopes.iter().rev().find_map(|s| s.get(&cap).copied())
                {
                    debug_assert!(l.captured, "resolver missed a capture");
                    if l.cell {
                        UpvalSrc::ParentLocal(l.reg)
                    } else {
                        UpvalSrc::ParentLocalValue(l.reg)
                    }
                } else if let Some(i) =
                    parent.upvals.iter().position(|(n, _)| *n == cap)
                {
                    UpvalSrc::ParentUpval(i as u8)
                } else {
                    // not visible at this point (e.g. declared later in the
                    // parent) — matches the emission-order semantics: the
                    // reference will resolve as a global at emit time
                    continue;
                };
                fs.upvals.push((cap, src));
            }
        }
        fs.arity = params.items.len() as u8;

        // M6c: direct children of <main> defer body compilation to first
        // call. Header (arity, upvals, annotations) is fully known here;
        // parameter subset errors still report at startup via the scan.
        if self.lazy && self.fs.len() == 1 {
            if let (Some((start, end, wrap)), Some(src)) = (lazy_span, self.source.clone()) {
                let arg_types = param_hints(params)
                    .map_err(|sp| CompileError {
                        msg: "not supported in M1: destructuring parameters".into(),
                        span_start: sp,
                    })?;
                let env: Vec<String> =
                    fs.upvals.iter().map(|(n, _)| n.clone()).collect();
                let upvals: Vec<UpvalSrc> =
                    fs.upvals.into_iter().map(|(_, u)| u).collect();
                let payload = LazyPayload {
                    source: src,
                    start,
                    end,
                    wrap,
                    name: name.to_string(),
                    is_async,
                    env,
                    src_name: self.src_name.clone(),
                    module: self.module.clone(),
                };
                let proto = Arc::new(FunctionProto::new_lazy(
                    Arc::from(name),
                    params.items.len() as u8,
                    is_async,
                    upvals,
                    arg_types,
                    tsc_ir::LazySource { payload: Arc::new(payload), fill: fill_lazy },
                ));
                proto.set_source_name(&self.src_name);
                let parent = self.f();
                parent.protos.push(proto);
                return Ok((parent.protos.len() - 1) as u16);
            }
        }

        self.fs.push(fs);
        self.emit_function_body(params, body, expr_body)?;
        let fs = self.fs.pop().unwrap();
        let proto = Arc::new(finish(fs));
        proto.set_source_name(&self.src_name);
        let parent = self.f();
        parent.protos.push(proto);
        Ok((parent.protos.len() - 1) as u16)
    }

    /// Parameters + body + implicit return, into the already-pushed
    /// FuncState. Shared by eager compiles and lazy fills.
    fn emit_function_body(
        &mut self,
        params: &FormalParameters,
        body: &[Statement],
        expr_body: Option<&Expression>,
    ) -> R {
        // arity pins registers 0..arity through the optimizer — must be
        // set before finish_body regardless of how this FuncState was built
        self.f().arity = params.items.len() as u8;
        for p in &params.items {
            let BindingPattern::BindingIdentifier(b) = &p.pattern else {
                return self.unsupported("destructuring parameters", p.span.start);
            };
            let hint = match &p.type_annotation {
                Some(ann) => match &ann.type_annotation {
                    TSType::TSNumberKeyword(_) => tsc_ir::TypeHint::Num,
                    TSType::TSBooleanKeyword(_) => tsc_ir::TypeHint::Bool,
                    TSType::TSStringKeyword(_) => tsc_ir::TypeHint::Str,
                    _ => tsc_ir::TypeHint::Top,
                },
                None => tsc_ir::TypeHint::Top,
            };
            self.f().arg_types.push(hint);
            let local = self.declare_local(&b.name, b.span.start)?;
            if local.cell {
                self.emit(Op::NewCell, local.reg, 0, 0);
            }
        }
        match expr_body {
            Some(e) => {
                let tmp = self.alloc_reg(e.span().start)?;
                self.expr(e, tmp)?;
                self.emit(Op::Return, tmp, 0, 0);
            }
            None => {
                self.hoist_functions(body)?;
                for s in body {
                    self.stmt(s)?;
                }
            }
        }
        // implicit `return undefined`
        let tmp = self.mark();
        let f = self.f();
        f.max_reg = f.max_reg.max(tmp + 1);
        self.emit(Op::LoadUndef, tmp, 0, 0);
        self.emit(Op::Return, tmp, 0, 0);
        Ok(())
    }

    // ---------------- expressions ----------------

    fn expr(&mut self, e: &Expression, dst: u8) -> R {
        self.discard_cur = std::mem::take(&mut self.discard_next);
        self.cur_span = e.span().start;
        match e {
            Expression::NumericLiteral(n) => {
                self.emit_number(n.value, dst);
                Ok(())
            }
            Expression::BooleanLiteral(b) => {
                self.emit(Op::LoadBool, dst, b.value as u8, 0);
                Ok(())
            }
            Expression::NullLiteral(_) => {
                self.emit(Op::LoadNull, dst, 0, 0);
                Ok(())
            }
            Expression::StringLiteral(s) => {
                let k = self.str_const(&s.value);
                self.emit_abx(Op::LoadConst, dst, k);
                Ok(())
            }
            Expression::Identifier(id) => {
                match &*id.name {
                    "undefined" => {
                        self.emit(Op::LoadUndef, dst, 0, 0);
                    }
                    "NaN" => {
                        let k = self.konst(Const::Number(f64::NAN));
                        self.emit_abx(Op::LoadConst, dst, k);
                    }
                    "Infinity" => {
                        let k = self.konst(Const::Number(f64::INFINITY));
                        self.emit_abx(Op::LoadConst, dst, k);
                    }
                    name => {
                        let place = self.resolve(name);
                        self.load_place(place, name, dst);
                    }
                }
                Ok(())
            }
            Expression::TemplateLiteral(t) => self.template(t, dst),
            Expression::ParenthesizedExpression(p) => self.expr(&p.expression, dst),
            Expression::AwaitExpression(aw) => {
                if !self.fs.last().unwrap().is_async {
                    return self.unsupported(
                        "await outside an async function",
                        aw.span.start,
                    );
                }
                self.expr(&aw.argument, dst)?;
                self.cur_span = aw.span.start;
                self.emit(Op::Await, dst, 0, 0);
                Ok(())
            }
            Expression::TSAsExpression(t) => self.expr(&t.expression, dst),
            Expression::TSNonNullExpression(t) => self.expr(&t.expression, dst),

            Expression::BinaryExpression(b) => {
                let op = match b.operator {
                    BinaryOperator::Addition => Op::Add,
                    BinaryOperator::Subtraction => Op::Sub,
                    BinaryOperator::Multiplication => Op::Mul,
                    BinaryOperator::Division => Op::Div,
                    BinaryOperator::Remainder => Op::Mod,
                    BinaryOperator::Exponential => Op::Pow,
                    BinaryOperator::Equality | BinaryOperator::StrictEquality => Op::Eq,
                    BinaryOperator::Inequality | BinaryOperator::StrictInequality => {
                        Op::Ne
                    }
                    BinaryOperator::LessThan => Op::Lt,
                    BinaryOperator::LessEqualThan => Op::Le,
                    BinaryOperator::GreaterThan => Op::Gt,
                    BinaryOperator::GreaterEqualThan => Op::Ge,
                    BinaryOperator::BitwiseAnd => Op::BitAnd,
                    BinaryOperator::BitwiseOR => Op::BitOr,
                    BinaryOperator::BitwiseXOR => Op::BitXor,
                    BinaryOperator::ShiftLeft => Op::Shl,
                    BinaryOperator::ShiftRight => Op::Shr,
                    BinaryOperator::ShiftRightZeroFill => Op::UShr,
                    BinaryOperator::In | BinaryOperator::Instanceof => {
                        return self.unsupported("in/instanceof", b.span.start)
                    }
                };
                let mark = self.mark();
                // operands read local registers directly when safe
                let rb = match self.local_operand(&b.left) {
                    Some(r) if Self::is_effect_free(&b.right) => r,
                    _ => {
                        let r = self.alloc_reg(b.span.start)?;
                        self.expr(&b.left, r)?;
                        r
                    }
                };
                let rc = match self.local_operand(&b.right) {
                    Some(r) => r,
                    None => {
                        let r = self.alloc_reg(b.span.start)?;
                        self.expr(&b.right, r)?;
                        r
                    }
                };
                self.cur_span = b.span.start;
                self.emit(op, dst, rb, rc);
                self.free_to(mark);
                Ok(())
            }
            Expression::LogicalExpression(l) => {
                self.expr(&l.left, dst)?;
                let j = match l.operator {
                    LogicalOperator::And => self.emit_jump(Op::JumpIfFalse, dst),
                    LogicalOperator::Or => self.emit_jump(Op::JumpIfTrue, dst),
                    LogicalOperator::Coalesce => {
                        return self.unsupported("?? operator", l.span.start)
                    }
                };
                self.expr(&l.right, dst)?;
                self.patch_here(j);
                Ok(())
            }
            Expression::UnaryExpression(u) => match u.operator {
                UnaryOperator::UnaryNegation => {
                    let mark = self.mark();
                    let rb = self.alloc_reg(u.span.start)?;
                    self.expr(&u.argument, rb)?;
                    self.emit(Op::Neg, dst, rb, 0);
                    self.free_to(mark);
                    Ok(())
                }
                UnaryOperator::LogicalNot => {
                    let mark = self.mark();
                    let rb = self.alloc_reg(u.span.start)?;
                    self.expr(&u.argument, rb)?;
                    self.emit(Op::Not, dst, rb, 0);
                    self.free_to(mark);
                    Ok(())
                }
                UnaryOperator::BitwiseNot => {
                    let mark = self.mark();
                    let rb = self.alloc_reg(u.span.start)?;
                    self.expr(&u.argument, rb)?;
                    self.emit(Op::BitNot, dst, rb, 0);
                    self.free_to(mark);
                    Ok(())
                }
                UnaryOperator::Typeof => {
                    let mark = self.mark();
                    let rb = self.alloc_reg(u.span.start)?;
                    self.expr(&u.argument, rb)?;
                    self.emit(Op::TypeOf, dst, rb, 0);
                    self.free_to(mark);
                    Ok(())
                }
                _ => self.unsupported("unary operator", u.span.start),
            },
            Expression::UpdateExpression(u) => {
                let SimpleAssignmentTarget::AssignmentTargetIdentifier(id) = &u.argument
                else {
                    return self.unsupported("++/-- on non-identifier", u.span.start);
                };
                let name = id.name.to_string();
                let place = self.resolve(&name);
                let mark = self.mark();
                let cur = self.alloc_reg(u.span.start)?;
                self.load_place(place, &name, cur);
                let one = self.alloc_reg(u.span.start)?;
                self.emit_abx(Op::LoadInt, one, (tsc_ir::SBX_BIAS + 1) as u16);
                let op = match u.operator {
                    UpdateOperator::Increment => Op::Add,
                    UpdateOperator::Decrement => Op::Sub,
                };
                if u.prefix {
                    self.emit(op, dst, cur, one);
                    self.store_place(place, &name, dst, u.span.start)?;
                } else {
                    if cur != dst {
                        self.emit(Op::Move, dst, cur, 0);
                    }
                    self.emit(op, cur, cur, one);
                    self.store_place(place, &name, cur, u.span.start)?;
                }
                self.free_to(mark);
                Ok(())
            }
            Expression::AssignmentExpression(a) => self.assignment(a, dst),
            Expression::ConditionalExpression(c) => {
                let mark = self.mark();
                let cond = self.alloc_reg(c.span.start)?;
                self.expr(&c.test, cond)?;
                let jf = self.emit_jump(Op::JumpIfFalse, cond);
                self.free_to(mark);
                self.expr(&c.consequent, dst)?;
                let jend = self.emit_jump(Op::Jump, 0);
                self.patch_here(jf);
                self.expr(&c.alternate, dst)?;
                self.patch_here(jend);
                Ok(())
            }
            Expression::CallExpression(c) => self.call(c, dst),
            Expression::ImportExpression(imp) => {
                // dynamic import(): hidden native __import(spec, importerId)
                let mark = self.mark();
                let f = self.alloc_reg(imp.span.start)?;
                let k = self.str_const("__import");
                self.emit_abx(Op::GetGlobal, f, k);
                let spec = self.alloc_reg(imp.span.start)?;
                self.expr(&imp.source, spec)?;
                let importer = self.alloc_reg(imp.span.start)?;
                let src_name = self.src_name.clone();
                let ik = self.str_const(&src_name);
                self.emit_abx(Op::LoadConst, importer, ik);
                self.cur_span = imp.span.start;
                self.emit(Op::Call, f, 2, 0);
                self.emit(Op::Move, dst, f, 0);
                self.free_to(mark);
                return Ok(());
            }
            Expression::StaticMemberExpression(m) => {
                // import.meta.url / .dirname: constant-folded per module.
                // Only member reads are supported — import.meta as a value
                // would need a real object with no use for one yet.
                if matches!(&m.object, Expression::MetaProperty(_)) {
                    let Some(module) = &self.module else {
                        return self
                            .unsupported("import.meta (module pipeline only)", m.span.start);
                    };
                    let path = module
                        .key
                        .strip_prefix("\0mod:")
                        .unwrap_or(&module.key)
                        .to_string();
                    let text = match &*m.property.name {
                        "url" => format!("file://{path}"),
                        "dirname" => std::path::Path::new(&path)
                            .parent()
                            .unwrap_or(std::path::Path::new(""))
                            .display()
                            .to_string(),
                        "filename" => path,
                        other => {
                            return self.unsupported(
                                &format!("import.meta.{other}"),
                                m.span.start,
                            )
                        }
                    };
                    let k = self.str_const(&text);
                    self.emit_abx(Op::LoadConst, dst, k);
                    return Ok(());
                }
                let mark = self.mark();
                let obj = self.alloc_reg(m.span.start)?;
                self.expr(&m.object, obj)?;
                if m.property.name == "length" {
                    self.emit(Op::Len, dst, obj, 0);
                } else {
                    let k = self.str_const(&m.property.name);
                    if k > u8::MAX as u16 {
                        return self.unsupported("too many constants", m.span.start);
                    }
                    self.emit(Op::GetField, dst, obj, k as u8);
                    // ns cells deref inside GetField itself (helper path)
                }
                self.free_to(mark);
                Ok(())
            }
            Expression::ComputedMemberExpression(m) => {
                let mark = self.mark();
                let obj = self.alloc_reg(m.span.start)?;
                self.expr(&m.object, obj)?;
                let idx = self.alloc_reg(m.span.start)?;
                self.expr(&m.expression, idx)?;
                self.emit(Op::GetIndex, dst, obj, idx);
                self.free_to(mark);
                Ok(())
            }
            Expression::ArrayExpression(a) => {
                let n = a.elements.len();
                // fused literal: evaluate elements into contiguous regs,
                // allocate + copy in one op (small literals only — keeps
                // register pressure bounded)
                if n > 0 && n <= 8 && a.elements.iter().all(|el| el.as_expression().is_some())
                {
                    let mark = self.mark();
                    let first = self.mark();
                    for el in &a.elements {
                        let r = self.alloc_reg(a.span.start)?;
                        self.expr(el.as_expression().unwrap(), r)?;
                    }
                    self.emit(Op::NewArrayLit, dst, first, n as u8);
                    self.free_to(mark);
                    return Ok(());
                }
                let n = n.min(u8::MAX as usize) as u8;
                self.emit(Op::NewArray, dst, n, 0);
                let mark = self.mark();
                let tmp = self.alloc_reg(a.span.start)?;
                for el in &a.elements {
                    let Some(e) = el.as_expression() else {
                        return self.unsupported("array holes/spread", a.span.start);
                    };
                    self.expr(e, tmp)?;
                    self.emit(Op::ArrayPush, dst, tmp, 0);
                }
                self.free_to(mark);
                Ok(())
            }
            Expression::ObjectExpression(o) => {
                // fused literal: all-static small literals evaluate values
                // into contiguous regs, then allocate with the final shape
                // in one op (per-pc shape cache fills at runtime)
                let n = o.properties.len();
                let static_keys: Option<Vec<Arc<str>>> = if n > 0 && n <= 8 {
                    o.properties
                        .iter()
                        .map(|p| match p {
                            ObjectPropertyKind::ObjectProperty(p) => match &p.key {
                                PropertyKey::StaticIdentifier(id) => {
                                    Some(Arc::from(id.name.as_str()))
                                }
                                PropertyKey::StringLiteral(s) => {
                                    Some(Arc::from(s.value.as_str()))
                                }
                                _ => None,
                            },
                            _ => None,
                        })
                        .collect()
                } else {
                    None
                };
                if let Some(keys) = static_keys {
                    let k = self.konst(Const::Keys(keys.into()));
                    if k <= u8::MAX as u16 {
                        let mark = self.mark();
                        let first = self.mark();
                        for p in &o.properties {
                            let ObjectPropertyKind::ObjectProperty(p) = p else {
                                unreachable!()
                            };
                            let r = self.alloc_reg(o.span.start)?;
                            self.expr(&p.value, r)?;
                        }
                        self.emit(Op::NewObjectLit, dst, first, k as u8);
                        self.free_to(mark);
                        return Ok(());
                    }
                }
                self.emit(Op::NewObject, dst, 0, 0);
                let mark = self.mark();
                let tmp = self.alloc_reg(o.span.start)?;
                for p in &o.properties {
                    let ObjectPropertyKind::ObjectProperty(p) = p else {
                        return self.unsupported("object spread", o.span.start);
                    };
                    let key = match &p.key {
                        PropertyKey::StaticIdentifier(id) => id.name.to_string(),
                        PropertyKey::StringLiteral(s) => s.value.to_string(),
                        _ => return self.unsupported("computed keys", p.span.start),
                    };
                    self.expr(&p.value, tmp)?;
                    let k = self.str_const(&key);
                    if k > u8::MAX as u16 {
                        return self.unsupported("too many constants", p.span.start);
                    }
                    self.emit(Op::SetField, dst, k as u8, tmp);
                }
                self.free_to(mark);
                Ok(())
            }
            Expression::ArrowFunctionExpression(a) => {
                // expression-bodied arrows (`x => x * 2`): oxc wraps the
                // expression in an ExpressionStatement — lower it as a return
                let expr_body = if a.expression {
                    match a.body.statements.first() {
                        Some(Statement::ExpressionStatement(e)) => Some(&e.expression),
                        _ => None,
                    }
                } else {
                    None
                };
                let idx = self.compile_function_inner(
                    a.span.start,
                    "<arrow>",
                    &a.params,
                    &a.body.statements,
                    expr_body,
                    a.r#async,
                    Some((a.span.start, a.span.end, true)),
                )?;
                self.emit_abx(Op::Closure, dst, idx);
                Ok(())
            }
            Expression::FunctionExpression(f) => {
                let Some(body) = &f.body else {
                    return self.unsupported("declare function", f.span.start);
                };
                let name = f.id.as_ref().map(|i| i.name.to_string());
                let idx = self.compile_function(
                    f.span.start,
                    name.as_deref().unwrap_or("<anon>"),
                    &f.params,
                    &body.statements,
                    f.r#async,
                    Some((f.span.start, f.span.end, true)),
                )?;
                self.emit_abx(Op::Closure, dst, idx);
                Ok(())
            }
            other => self.unsupported(
                &format!("expression {:?}", expr_kind(other)),
                other.span().start,
            ),
        }
    }

    fn emit_number(&mut self, v: f64, dst: u8) {
        if v == v.trunc() && (-32768.0..=32767.0).contains(&v) && !(v == 0.0 && v.is_sign_negative())
        {
            self.emit_abx(Op::LoadInt, dst, (v as i32 + tsc_ir::SBX_BIAS) as u16);
        } else {
            let k = self.konst(Const::Number(v));
            self.emit_abx(Op::LoadConst, dst, k);
        }
    }

    fn template(&mut self, t: &TemplateLiteral, dst: u8) -> R {
        let mark = self.mark();
        let quasi0 = t.quasis[0].value.cooked.as_deref().unwrap_or("");
        let k = self.str_const(quasi0);
        self.emit_abx(Op::LoadConst, dst, k);
        let tmp = self.alloc_reg(t.span.start)?;
        for (i, e) in t.expressions.iter().enumerate() {
            self.expr(e, tmp)?;
            self.emit(Op::Concat, dst, dst, tmp);
            let q = t.quasis[i + 1].value.cooked.as_deref().unwrap_or("");
            if !q.is_empty() {
                let k = self.str_const(q);
                self.emit_abx(Op::LoadConst, tmp, k);
                self.emit(Op::Concat, dst, dst, tmp);
            }
        }
        self.free_to(mark);
        Ok(())
    }

    fn assignment(&mut self, a: &AssignmentExpression, dst: u8) -> R {
        let bin_op = match a.operator {
            AssignmentOperator::Assign => None,
            AssignmentOperator::Addition => Some(Op::Add),
            AssignmentOperator::Subtraction => Some(Op::Sub),
            AssignmentOperator::Multiplication => Some(Op::Mul),
            AssignmentOperator::Division => Some(Op::Div),
            AssignmentOperator::Remainder => Some(Op::Mod),
            AssignmentOperator::BitwiseAnd => Some(Op::BitAnd),
            AssignmentOperator::BitwiseOR => Some(Op::BitOr),
            AssignmentOperator::BitwiseXOR => Some(Op::BitXor),
            AssignmentOperator::ShiftLeft => Some(Op::Shl),
            AssignmentOperator::ShiftRight => Some(Op::Shr),
            AssignmentOperator::ShiftRightZeroFill => Some(Op::UShr),
            _ => return self.unsupported("assignment operator", a.span.start),
        };
        match &a.left {
            AssignmentTarget::AssignmentTargetIdentifier(id) => {
                let name = id.name.to_string();
                let place = self.resolve(&name);
                match bin_op {
                    None => {
                        self.expr(&a.right, dst)?;
                    }
                    Some(op) => {
                        let mark = self.mark();
                        let cur = self.alloc_reg(a.span.start)?;
                        self.load_place(place, &name, cur);
                        let rhs = self.alloc_reg(a.span.start)?;
                        self.expr(&a.right, rhs)?;
                        self.cur_span = a.span.start;
                        self.emit(op, dst, cur, rhs);
                        self.free_to(mark);
                    }
                }
                self.store_place(place, &name, dst, a.span.start)?;
                Ok(())
            }
            AssignmentTarget::StaticMemberExpression(m) => {
                let mark = self.mark();
                let obj = self.alloc_reg(m.span.start)?;
                self.expr(&m.object, obj)?;
                let k = self.str_const(&m.property.name);
                if k > u8::MAX as u16 {
                    return self.unsupported("too many constants", m.span.start);
                }
                match bin_op {
                    None => self.expr(&a.right, dst)?,
                    Some(op) => {
                        let cur = self.alloc_reg(a.span.start)?;
                        self.emit(Op::GetField, cur, obj, k as u8);
                        let rhs = self.alloc_reg(a.span.start)?;
                        self.expr(&a.right, rhs)?;
                        self.cur_span = a.span.start;
                        self.emit(op, dst, cur, rhs);
                    }
                }
                self.emit(Op::SetField, obj, k as u8, dst);
                self.free_to(mark);
                Ok(())
            }
            AssignmentTarget::ComputedMemberExpression(m) => {
                let mark = self.mark();
                let obj = self.alloc_reg(m.span.start)?;
                self.expr(&m.object, obj)?;
                let idx = self.alloc_reg(m.span.start)?;
                self.expr(&m.expression, idx)?;
                match bin_op {
                    None => self.expr(&a.right, dst)?,
                    Some(op) => {
                        let cur = self.alloc_reg(a.span.start)?;
                        self.emit(Op::GetIndex, cur, obj, idx);
                        let rhs = self.alloc_reg(a.span.start)?;
                        self.expr(&a.right, rhs)?;
                        self.cur_span = a.span.start;
                        self.emit(op, dst, cur, rhs);
                    }
                }
                self.emit(Op::SetIndex, obj, idx, dst);
                self.free_to(mark);
                Ok(())
            }
            _ => self.unsupported("destructuring assignment", a.span.start),
        }
    }

    fn call(&mut self, c: &CallExpression, dst: u8) -> R {
        // method special cases: arr.push(v), s.charCodeAt(i)
        if let Expression::StaticMemberExpression(m) = &c.callee {
            match &*m.property.name {
                "push" if c.arguments.len() == 1 => {
                    let discard = self.discard_cur;
                    let mark = self.mark();
                    let recv = self.alloc_reg(m.span.start)?;
                    self.expr(&m.object, recv)?;
                    let arg = self.alloc_reg(m.span.start)?;
                    let e = c.arguments[0]
                        .as_expression()
                        .ok_or_else(|| CompileError {
                            msg: "not supported in M1: spread argument".into(),
                            span_start: c.span.start,
                        })?;
                    self.expr(e, arg)?;
                    self.emit(Op::ArrayPush, recv, arg, 0);
                    if !discard {
                        self.emit(Op::Len, dst, recv, 0); // push returns new length
                    }
                    self.free_to(mark);
                    return Ok(());
                }
                "charCodeAt" if c.arguments.len() == 1 => {
                    // lowered to hidden native __charCodeAt(recv, i)
                    let mark = self.mark();
                    let f = self.alloc_reg(m.span.start)?;
                    let k = self.str_const("__charCodeAt");
                    self.emit_abx(Op::GetGlobal, f, k);
                    let recv = self.alloc_reg(m.span.start)?;
                    self.expr(&m.object, recv)?;
                    let arg = self.alloc_reg(m.span.start)?;
                    let e = c.arguments[0]
                        .as_expression()
                        .ok_or_else(|| CompileError {
                            msg: "not supported in M1: spread argument".into(),
                            span_start: c.span.start,
                        })?;
                    self.expr(e, arg)?;
                    self.cur_span = c.span.start;
                    self.emit(Op::Call, f, 2, 0);
                    self.emit(Op::Move, dst, f, 0);
                    self.free_to(mark);
                    return Ok(());
                }
                _ => {}
            }
        }
        let mark = self.mark();
        let f = self.alloc_reg(c.span.start)?;
        self.expr(&c.callee, f)?;
        for arg in &c.arguments {
            let Some(e) = arg.as_expression() else {
                return self.unsupported("spread argument", c.span.start);
            };
            let r = self.alloc_reg(c.span.start)?;
            self.expr(e, r)?;
        }
        self.cur_span = c.span.start;
        self.emit(Op::Call, f, c.arguments.len() as u8, 0);
        if f != dst {
            self.emit(Op::Move, dst, f, 0);
        }
        self.free_to(mark);
        Ok(())
    }
}

/// Parameter type hints without emission (lazy header construction);
/// errors with the span of an unsupported pattern.
fn param_hints(params: &FormalParameters) -> Result<Vec<tsc_ir::TypeHint>, u32> {
    let mut hints = Vec::with_capacity(params.items.len());
    for p in &params.items {
        let BindingPattern::BindingIdentifier(_) = &p.pattern else {
            return Err(p.span.start);
        };
        hints.push(match &p.type_annotation {
            Some(ann) => match &ann.type_annotation {
                TSType::TSNumberKeyword(_) => tsc_ir::TypeHint::Num,
                TSType::TSBooleanKeyword(_) => tsc_ir::TypeHint::Bool,
                TSType::TSStringKeyword(_) => tsc_ir::TypeHint::Str,
                _ => tsc_ir::TypeHint::Top,
            },
            None => tsc_ir::TypeHint::Top,
        });
    }
    Ok(hints)
}

/// Optimizer + body construction (shared by eager finish and lazy fills).
fn finish_body(mut fs: FuncState) -> tsc_ir::ProtoBody {
    let upval_srcs: Vec<Vec<tsc_ir::UpvalSrc>> =
        fs.protos.iter().map(|p| p.upvals.clone()).collect();
    tsc_optimizer::optimize(
        &mut fs.code,
        &mut fs.spans,
        &fs.consts,
        &upval_srcs,
        fs.max_reg.max(1),
        fs.arity,
    );
    tsc_ir::ProtoBody {
        n_regs: fs.max_reg.max(1),
        code: fs.code,
        consts: fs.consts,
        protos: fs.protos,
        spans: fs.spans,
    }
}

fn finish(fs: FuncState) -> FunctionProto {
    let name: Arc<str> = Arc::from(fs.name.as_str());
    let arity = fs.arity;
    let is_async = fs.is_async;
    let upvals = fs.upvals.iter().map(|(_, s)| *s).collect();
    let arg_types = fs.arg_types.clone();
    FunctionProto::new(name, arity, is_async, upvals, arg_types, finish_body(fs))
}

fn stmt_kind(s: &Statement) -> &'static str {
    match s {
        Statement::ThrowStatement(_) => "throw",
        Statement::TryStatement(_) => "try/catch",
        Statement::ClassDeclaration(_) => "class",
        Statement::SwitchStatement(_) => "switch",
        Statement::DoWhileStatement(_) => "do-while",
        Statement::ForInStatement(_) => "for-in",
        Statement::ImportDeclaration(_) => "import (single-file entry only)",
        Statement::ExportNamedDeclaration(_) | Statement::ExportDefaultDeclaration(_) => {
            "export"
        }
        _ => "this statement",
    }
}

fn expr_kind(e: &Expression) -> &'static str {
    match e {
        Expression::NewExpression(_) => "new",
        Expression::ThisExpression(_) => "this",
        Expression::AwaitExpression(_) => "await",
        Expression::ClassExpression(_) => "class",
        Expression::RegExpLiteral(_) => "regex",
        Expression::SequenceExpression(_) => "comma expression",
        _ => "this expression",
    }
}
