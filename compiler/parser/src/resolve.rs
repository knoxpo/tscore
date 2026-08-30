//! Capture analysis: which bindings are referenced from a nested function.
//! Walks exactly the TS-M1 subset (the emitter rejects everything else, so
//! unknown nodes are simply not descended here).

use std::collections::{HashMap, HashSet};
use tsc_ast::oxc_ast::ast::*;

/// Binding identity = span start of its BindingIdentifier.
pub type BindingId = u32;

struct Scope {
    names: HashMap<String, (BindingId, usize)>,
}

pub struct Resolver {
    scopes: Vec<Scope>,
    fn_depth: usize,
    pub captured: HashSet<BindingId>,
    pub mutated: HashSet<BindingId>,
}

impl Resolver {
    pub fn run(program: &Program) -> (HashSet<BindingId>, HashSet<BindingId>) {
        let mut r = Resolver {
            scopes: vec![Scope { names: HashMap::new() }],
            fn_depth: 0,
            captured: HashSet::new(),
            mutated: HashSet::new(),
        };
        r.hoist_functions(&program.body);
        for s in &program.body {
            r.stmt(s);
        }
        (r.captured, r.mutated)
    }

    fn mark_mutated(&mut self, name: &str) {
        for scope in self.scopes.iter().rev() {
            if let Some(&(id, _)) = scope.names.get(name) {
                self.mutated.insert(id);
                return;
            }
        }
    }

    fn declare(&mut self, name: &str, id: BindingId) {
        let depth = self.fn_depth;
        self.scopes
            .last_mut()
            .unwrap()
            .names
            .insert(name.to_string(), (id, depth));
    }

    fn reference(&mut self, name: &str) {
        for scope in self.scopes.iter().rev() {
            if let Some(&(id, depth)) = scope.names.get(name) {
                if depth < self.fn_depth {
                    self.captured.insert(id);
                }
                return;
            }
        }
    }

    fn hoist_functions(&mut self, stmts: &[Statement]) {
        for s in stmts {
            if let Statement::FunctionDeclaration(f) = s {
                if let Some(id) = &f.id {
                    self.declare(&id.name, id.span.start);
                    // the declaration itself stores the closure after hoist
                    self.mutated.insert(id.span.start);
                }
            }
        }
    }

    fn function(&mut self, params: &FormalParameters, body: &FunctionBody) {
        self.fn_depth += 1;
        self.scopes.push(Scope { names: HashMap::new() });
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
        self.fn_depth -= 1;
    }

    fn stmt(&mut self, s: &Statement) {
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
                    self.function(&f.params, body);
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
                self.scopes.push(Scope { names: HashMap::new() });
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
                self.scopes.push(Scope { names: HashMap::new() });
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
                self.scopes.push(Scope { names: HashMap::new() });
                self.hoist_functions(&b.body);
                for s in &b.body {
                    self.stmt(s);
                }
                self.scopes.pop();
            }
            _ => {}
        }
    }

    fn expr(&mut self, e: &Expression) {
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
                self.function(&a.params, &a.body);
            }
            Expression::FunctionExpression(f) => {
                if let Some(body) = &f.body {
                    self.function(&f.params, body);
                }
            }
            Expression::TSAsExpression(t) => self.expr(&t.expression),
            Expression::TSNonNullExpression(t) => self.expr(&t.expression),
            _ => {}
        }
    }
}
