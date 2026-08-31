//! Capture analysis: which bindings are referenced from a nested function.
//! Walks exactly the TS-M1 subset (the emitter rejects everything else, so
//! unknown nodes are simply not descended here).

use std::collections::{HashMap, HashSet};
use tsc_ast::oxc_ast::ast::*;

/// Binding identity = span start of its BindingIdentifier.
pub type BindingId = u32;

/// Function identity = span start of its Function/Arrow node.
pub type FnId = u32;

/// Small linear-scan scope: cheaper than a HashMap at typical sizes and
/// allocation-free (names borrow from the AST). Scopes that grow past
/// SCOPE_INDEX_AT (the 12k-function top level of a generated bundle)
/// get a side index.
const SCOPE_INDEX_AT: usize = 32;

struct Scope<'a> {
    names: Vec<(&'a str, (BindingId, usize))>,
    /// name -> index into `names` of its latest declaration.
    index: Option<HashMap<&'a str, usize>>,
}

impl<'a> Scope<'a> {
    fn new() -> Self {
        Scope { names: Vec::new(), index: None }
    }

    fn get(&self, name: &str) -> Option<(BindingId, usize)> {
        if let Some(ix) = &self.index {
            return ix.get(name).map(|&i| self.names[i].1);
        }
        self.names.iter().rev().find(|(n, _)| *n == name).map(|&(_, h)| h)
    }

    fn push(&mut self, name: &'a str, hit: (BindingId, usize)) {
        self.names.push((name, hit));
        if self.names.len() > SCOPE_INDEX_AT && self.index.is_none() {
            self.index = Some(
                self.names.iter().enumerate().map(|(i, &(n, _))| (n, i)).collect(),
            );
        }
        if let Some(ix) = &mut self.index {
            ix.insert(self.names.last().unwrap().0, self.names.len() - 1);
        }
    }
}

/// One active function during the walk: collects the ordered list of free
/// names it (or any descendant) references from enclosing scopes. This is
/// the child's upvalue table, computed without emitting its body (M6b —
/// the lazy-compile prerequisite).
struct FnRec {
    id: FnId,
    caps: Vec<String>,
}

pub struct Resolver<'a> {
    scopes: Vec<Scope<'a>>,
    fn_depth: usize,
    fn_stack: Vec<FnRec>,
    pub captured: HashSet<BindingId>,
    pub mutated: HashSet<BindingId>,
    /// Per-function ordered capture-name lists (first-reference order).
    pub fn_caps: HashMap<FnId, Vec<String>>,
}

impl<'a> Resolver<'a> {
    pub fn run(
        program: &'a Program<'a>,
    ) -> (HashSet<BindingId>, HashSet<BindingId>, HashMap<FnId, Vec<String>>) {
        Self::run_seeded(program, &[])
    }

    /// Like [`run`], with `env` names pre-declared in the outermost scope
    /// — used when re-resolving a lazily-compiled function snippet whose
    /// free names are the stored upvalue environment (M6c).
    pub fn run_seeded(
        program: &'a Program<'a>,
        env: &'a [String],
    ) -> (HashSet<BindingId>, HashSet<BindingId>, HashMap<FnId, Vec<String>>) {
        let mut r = Resolver {
            scopes: vec![Scope::new()],
            fn_depth: 0,
            fn_stack: Vec::new(),
            captured: HashSet::new(),
            mutated: HashSet::new(),
            fn_caps: HashMap::new(),
        };
        for (i, name) in env.iter().enumerate() {
            // synthetic binding ids well clear of real source spans
            r.declare(name, u32::MAX - i as u32);
        }
        r.hoist_functions(&program.body);
        for s in &program.body {
            r.stmt(s);
        }
        (r.captured, r.mutated, r.fn_caps)
    }

    fn lookup(&self, name: &str) -> Option<(BindingId, usize)> {
        for scope in self.scopes.iter().rev() {
            if let Some(hit) = scope.get(name) {
                return Some(hit);
            }
        }
        None
    }

    fn mark_mutated(&mut self, name: &str) {
        if let Some((id, _)) = self.lookup(name) {
            self.mutated.insert(id);
        }
    }

    fn declare(&mut self, name: &'a str, id: BindingId) {
        let depth = self.fn_depth;
        self.scopes.last_mut().unwrap().push(name, (id, depth));
    }

    fn reference(&mut self, name: &str) {
        if let Some((id, depth)) = self.lookup(name) {
            if depth < self.fn_depth {
                self.captured.insert(id);
                // thread the capture through every enclosing function
                // between the owner and the referencing one — mirrors
                // the emitter's upvalue chain
                for level in depth..self.fn_depth {
                    let rec = &mut self.fn_stack[level];
                    if !rec.caps.iter().any(|n| n == name) {
                        rec.caps.push(name.to_string());
                    }
                }
            }
        }
    }

    fn hoist_functions(&mut self, stmts: &'a [Statement<'a>]) {
        for s in stmts {
            let f = match s {
                Statement::FunctionDeclaration(f) => f,
                Statement::ExportNamedDeclaration(e) => match &e.declaration {
                    Some(tsc_ast::oxc_ast::ast::Declaration::FunctionDeclaration(f)) => f,
                    _ => continue,
                },
                Statement::ExportDefaultDeclaration(e) => match &e.declaration {
                    tsc_ast::oxc_ast::ast::ExportDefaultDeclarationKind::FunctionDeclaration(f) => f,
                    _ => continue,
                },
                _ => continue,
            };
            {
                if let Some(id) = &f.id {
                    self.declare(&id.name, id.span.start);
                    // the declaration itself stores the closure after hoist
                    self.mutated.insert(id.span.start);
                }
            }
        }
    }

    fn function(&mut self, id: FnId, params: &'a FormalParameters<'a>, body: &'a FunctionBody<'a>) {
        self.fn_depth += 1;
        self.fn_stack.push(FnRec { id, caps: Vec::new() });
        self.scopes.push(Scope::new());
        for p in &params.items {
            if let BindingPattern::BindingIdentifier(b) = &p.pattern {
                self.declare(&b.name, b.span.start);
            }
        }
        self.hoist_functions(&body.statements);
        for s in &body.statements {
            self.stmt(s);
        }
        self.scopes.pop();
        let rec = self.fn_stack.pop().unwrap();
        if !rec.caps.is_empty() {
            self.fn_caps.insert(rec.id, rec.caps);
        }
        self.fn_depth -= 1;
    }

    fn stmt(&mut self, s: &'a Statement<'a>) {
        match s {
            Statement::VariableDeclaration(d) => {
                for decl in &d.declarations {
                    if let Some(init) = &decl.init {
                        self.expr(init);
                    }
                    if let BindingPattern::BindingIdentifier(b) = &decl.id {
                        self.declare(&b.name, b.span.start);
                    }
                }
            }
            Statement::FunctionDeclaration(f) => {
                // name already hoisted
                if let Some(body) = &f.body {
                    self.function(f.span.start, &f.params, body);
                }
            }
            Statement::ExpressionStatement(e) => self.expr(&e.expression),
            Statement::ReturnStatement(r) => {
                if let Some(arg) = &r.argument {
                    self.expr(arg);
                }
            }
            Statement::IfStatement(i) => {
                self.expr(&i.test);
                self.stmt(&i.consequent);
                if let Some(alt) = &i.alternate {
                    self.stmt(alt);
                }
            }
            Statement::WhileStatement(w) => {
                self.expr(&w.test);
                self.stmt(&w.body);
            }
            Statement::ForStatement(f) => {
                self.scopes.push(Scope::new());
                match &f.init {
                    Some(ForStatementInit::VariableDeclaration(d)) => {
                        for decl in &d.declarations {
                            if let Some(init) = &decl.init {
                                self.expr(init);
                            }
                            if let BindingPattern::BindingIdentifier(b) = &decl.id {
                                self.declare(&b.name, b.span.start);
                            }
                        }
                    }
                    Some(init) => {
                        if let Some(e) = init.as_expression() {
                            self.expr(e);
                        }
                    }
                    None => {}
                }
                if let Some(test) = &f.test {
                    self.expr(test);
                }
                if let Some(update) = &f.update {
                    self.expr(update);
                }
                self.stmt(&f.body);
                self.scopes.pop();
            }
            Statement::ForOfStatement(f) => {
                self.expr(&f.right);
                self.scopes.push(Scope::new());
                if let ForStatementLeft::VariableDeclaration(d) = &f.left {
                    for decl in &d.declarations {
                        if let BindingPattern::BindingIdentifier(b) = &decl.id {
                            self.declare(&b.name, b.span.start);
                        }
                    }
                }
                self.stmt(&f.body);
                self.scopes.pop();
            }
            Statement::BlockStatement(b) => {
                self.scopes.push(Scope::new());
                self.hoist_functions(&b.body);
                for s in &b.body {
                    self.stmt(s);
                }
                self.scopes.pop();
            }
            // module forms: analyze the wrapped declaration/expression
            Statement::ExportNamedDeclaration(e) => {
                if let Some(d) = &e.declaration {
                    match d {
                        tsc_ast::oxc_ast::ast::Declaration::VariableDeclaration(v) => {
                            for decl in &v.declarations {
                                if let Some(init) = &decl.init {
                                    self.expr(init);
                                }
                                if let BindingPattern::BindingIdentifier(b) = &decl.id {
                                    self.declare(&b.name, b.span.start);
                                }
                            }
                        }
                        tsc_ast::oxc_ast::ast::Declaration::FunctionDeclaration(f) => {
                            if let Some(body) = &f.body {
                                self.function(f.span.start, &f.params, body);
                            }
                        }
                        _ => {}
                    }
                }
                for sp in &e.specifiers {
                    // `export { x }` reads x
                    self.reference(sp.local.name().as_str());
                }
            }
            Statement::ExportDefaultDeclaration(e) => {
                use tsc_ast::oxc_ast::ast::ExportDefaultDeclarationKind as K;
                match &e.declaration {
                    K::FunctionDeclaration(f) => {
                        if let Some(body) = &f.body {
                            self.function(f.span.start, &f.params, body);
                        }
                    }
                    other => {
                        if let Some(expr) = other.as_expression() {
                            self.expr(expr);
                        }
                    }
                }
            }
            Statement::ImportDeclaration(_) => {}
            _ => {}
        }
    }

    fn expr(&mut self, e: &'a Expression<'a>) {
        match e {
            Expression::Identifier(id) => self.reference(&id.name),
            Expression::BinaryExpression(b) => {
                self.expr(&b.left);
                self.expr(&b.right);
            }
            Expression::LogicalExpression(l) => {
                self.expr(&l.left);
                self.expr(&l.right);
            }
            Expression::UnaryExpression(u) => self.expr(&u.argument),
            Expression::AwaitExpression(a) => self.expr(&a.argument),
            Expression::UpdateExpression(u) => {
                if let SimpleAssignmentTarget::AssignmentTargetIdentifier(id) = &u.argument {
                    self.reference(&id.name);
                    self.mark_mutated(&id.name);
                }
            }
            Expression::AssignmentExpression(a) => {
                match &a.left {
                    AssignmentTarget::AssignmentTargetIdentifier(id) => {
                        self.reference(&id.name);
                        self.mark_mutated(&id.name);
                    }
                    AssignmentTarget::StaticMemberExpression(m) => self.expr(&m.object),
                    AssignmentTarget::ComputedMemberExpression(m) => {
                        self.expr(&m.object);
                        self.expr(&m.expression);
                    }
                    _ => {}
                }
                self.expr(&a.right);
            }
            Expression::ConditionalExpression(c) => {
                self.expr(&c.test);
                self.expr(&c.consequent);
                self.expr(&c.alternate);
            }
            Expression::CallExpression(c) => {
                self.expr(&c.callee);
                for arg in &c.arguments {
                    if let Some(e) = arg.as_expression() {
                        self.expr(e);
                    }
                }
            }
            Expression::StaticMemberExpression(m) => self.expr(&m.object),
            Expression::ComputedMemberExpression(m) => {
                self.expr(&m.object);
                self.expr(&m.expression);
            }
            Expression::ArrayExpression(a) => {
                for el in &a.elements {
                    if let Some(e) = el.as_expression() {
                        self.expr(e);
                    }
                }
            }
            Expression::ObjectExpression(o) => {
                for p in &o.properties {
                    if let ObjectPropertyKind::ObjectProperty(p) = p {
                        self.expr(&p.value);
                    }
                }
            }
            Expression::TemplateLiteral(t) => {
                for e in &t.expressions {
                    self.expr(e);
                }
            }
            Expression::ParenthesizedExpression(p) => self.expr(&p.expression),
            Expression::ArrowFunctionExpression(a) => {
                self.function(a.span.start, &a.params, &a.body);
            }
            Expression::FunctionExpression(f) => {
                if let Some(body) = &f.body {
                    self.function(f.span.start, &f.params, body);
                }
            }
            Expression::TSAsExpression(t) => self.expr(&t.expression),
            Expression::TSNonNullExpression(t) => self.expr(&t.expression),
            _ => {}
        }
    }
}
