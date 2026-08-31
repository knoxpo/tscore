//! Startup subset scan (M6 leftover): a cheap statement-level walk that
//! restores eager reporting for the common "not supported in M1"
//! constructs even though function bodies compile lazily.
//!
//! Deliberately conservative: it flags only statement kinds the emitter
//! certainly rejects, so it can never produce a false startup error.
//! Expression-level rejections (spread, `new`, `in`, computed keys, …)
//! stay with the emitter — for a lazy body that means first-call time,
//! which is the documented safety net.

use tsc_ast::oxc_ast::ast::*;

/// Earliest unsupported statement in the tree, if any.
pub fn subset_scan(stmts: &[Statement]) -> Option<(String, u32)> {
    let mut worst: Option<(String, u32)> = None;
    walk_stmts(stmts, &mut worst);
    worst
}

fn note(worst: &mut Option<(String, u32)>, what: &str, span: u32) {
    if worst.as_ref().is_none_or(|(_, s)| span < *s) {
        *worst = Some((format!("not supported in M1: {what}"), span));
    }
}

fn walk_stmts(stmts: &[Statement], worst: &mut Option<(String, u32)>) {
    for s in stmts {
        walk_stmt(s, worst);
    }
}

fn walk_stmt(s: &Statement, worst: &mut Option<(String, u32)>) {
    match s {
        Statement::ThrowStatement(t) => note(worst, "throw", t.span.start),
        Statement::TryStatement(t) => note(worst, "try/catch", t.span.start),
        Statement::ClassDeclaration(c) => note(worst, "class", c.span.start),
        Statement::SwitchStatement(sw) => note(worst, "switch", sw.span.start),
        Statement::DoWhileStatement(d) => note(worst, "do-while", d.span.start),
        Statement::ForInStatement(f) => note(worst, "for-in", f.span.start),
        // import/export are handled by the module pipeline (the CLI routes
        // module-syntax entries there before this scan can matter)
        Statement::FunctionDeclaration(f) => {
            if let Some(body) = &f.body {
                walk_stmts(&body.statements, worst);
            }
        }
        Statement::BlockStatement(b) => walk_stmts(&b.body, worst),
        Statement::IfStatement(i) => {
            walk_expr(&i.test, worst);
            walk_stmt(&i.consequent, worst);
            if let Some(alt) = &i.alternate {
                walk_stmt(alt, worst);
            }
        }
        Statement::WhileStatement(w) => {
            walk_expr(&w.test, worst);
            walk_stmt(&w.body, worst);
        }
        Statement::ForStatement(f) => {
            if let Some(ForStatementInit::VariableDeclaration(d)) = &f.init {
                for decl in &d.declarations {
                    if let Some(init) = &decl.init {
                        walk_expr(init, worst);
                    }
                }
            }
            if let Some(t) = &f.test {
                walk_expr(t, worst);
            }
            if let Some(u) = &f.update {
                walk_expr(u, worst);
            }
            walk_stmt(&f.body, worst);
        }
        Statement::ForOfStatement(f) => {
            walk_expr(&f.right, worst);
            walk_stmt(&f.body, worst);
        }
        Statement::ExpressionStatement(e) => walk_expr(&e.expression, worst),
        Statement::ReturnStatement(r) => {
            if let Some(arg) = &r.argument {
                walk_expr(arg, worst);
            }
        }
        Statement::VariableDeclaration(d) => {
            for decl in &d.declarations {
                if let Some(init) = &decl.init {
                    walk_expr(init, worst);
                }
            }
        }
        _ => {}
    }
}

/// Descend only far enough to reach nested function bodies — expression
/// kinds themselves are the emitter's business.
fn walk_expr(e: &Expression, worst: &mut Option<(String, u32)>) {
    match e {
        Expression::ArrowFunctionExpression(a) => {
            walk_stmts(&a.body.statements, worst)
        }
        Expression::FunctionExpression(f) => {
            if let Some(body) = &f.body {
                walk_stmts(&body.statements, worst);
            }
        }
        Expression::BinaryExpression(b) => {
            walk_expr(&b.left, worst);
            walk_expr(&b.right, worst);
        }
        Expression::LogicalExpression(l) => {
            walk_expr(&l.left, worst);
            walk_expr(&l.right, worst);
        }
        Expression::UnaryExpression(u) => walk_expr(&u.argument, worst),
        Expression::AwaitExpression(a) => walk_expr(&a.argument, worst),
        Expression::AssignmentExpression(a) => walk_expr(&a.right, worst),
        Expression::ConditionalExpression(c) => {
            walk_expr(&c.test, worst);
            walk_expr(&c.consequent, worst);
            walk_expr(&c.alternate, worst);
        }
        Expression::CallExpression(c) => {
            walk_expr(&c.callee, worst);
            for arg in &c.arguments {
                if let Some(e) = arg.as_expression() {
                    walk_expr(e, worst);
                }
            }
        }
        Expression::StaticMemberExpression(m) => walk_expr(&m.object, worst),
        Expression::ComputedMemberExpression(m) => {
            walk_expr(&m.object, worst);
            walk_expr(&m.expression, worst);
        }
        Expression::ArrayExpression(a) => {
            for el in &a.elements {
                if let Some(e) = el.as_expression() {
                    walk_expr(e, worst);
                }
            }
        }
        Expression::ObjectExpression(o) => {
            for p in &o.properties {
                if let ObjectPropertyKind::ObjectProperty(p) = p {
                    walk_expr(&p.value, worst);
                }
            }
        }
        Expression::TemplateLiteral(t) => {
            for e in &t.expressions {
                walk_expr(e, worst);
            }
        }
        Expression::ParenthesizedExpression(p) => walk_expr(&p.expression, worst),
        Expression::TSAsExpression(t) => walk_expr(&t.expression, worst),
        Expression::TSNonNullExpression(t) => walk_expr(&t.expression, worst),
        _ => {}
    }
}
