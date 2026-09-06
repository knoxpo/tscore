//! tsc-modules
//!
//! ES module graph front end: specifier resolution (relative + bare via
//! tscore.json), BFS discovery with export metadata, and per-file
//! compilation into a `tsc_ir::Program`. Instantiation/evaluation live on
//! the runtime side (tsr-modules).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tsc_ast::oxc_ast::ast::{
    BindingPattern, Declaration, ImportDeclarationSpecifier, ImportOrExportKind,
    Statement,
};
use tsc_ir::{ExportMeta, Module, Program, ReExport};

#[derive(Debug)]
pub struct ModuleError {
    /// File the error is in.
    pub path: String,
    pub msg: String,
    pub span_start: u32,
}

/// Bare-specifier roots from tscore.json (walked up from the entry dir).
pub struct ResolveConfig {
    pub root: PathBuf,
    pub module_dirs: Vec<String>,
    /// Prefix mappings tried before moduleDirs: "@app/" -> "./src/".
    pub paths: Vec<(String, String)>,
}

impl ResolveConfig {
    /// Find tscore.json upward from the entry file's directory. Absent:
    /// root = entry dir, default dirs.
    pub fn discover(entry: &Path) -> ResolveConfig {
        let start = entry.parent().unwrap_or(Path::new("."));
        let mut dir = Some(start);
        while let Some(d) = dir {
            let cfg = d.join("tscore.json");
            if cfg.exists() {
                let text = std::fs::read_to_string(&cfg).unwrap_or_default();
                let dirs =
                    parse_module_dirs(&text).unwrap_or_else(|| vec!["modules".into()]);
                return ResolveConfig {
                    root: d.to_path_buf(),
                    module_dirs: dirs,
                    paths: parse_paths(&text),
                };
            }
            dir = d.parent();
        }
        ResolveConfig {
            root: start.to_path_buf(),
            module_dirs: vec!["modules".into()],
            paths: Vec::new(),
        }
    }
}

/// tscore.json: {"moduleDirs": ["modules"], "paths": {"@app/": "./src/"}}
fn parse_paths(s: &str) -> Vec<(String, String)> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(s) else {
        return Vec::new();
    };
    let Some(map) = v.get("paths").and_then(|p| p.as_object()) else {
        return Vec::new();
    };
    map.iter()
        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
        .collect()
}

fn parse_module_dirs(s: &str) -> Option<Vec<String>> {
    let v: serde_json::Value = serde_json::from_str(s).ok()?;
    Some(
        v.get("moduleDirs")?
            .as_array()?
            .iter()
            .filter_map(|d| d.as_str().map(str::to_string))
            .collect(),
    )
}

/// The entry point a package.json declares, as a path relative to the
/// package root. Subset of Node's algorithm: "exports" as a string, or as
/// an object taking "." then the first of "import"/"default"; else "main".
/// ponytail: no wildcards, no "imports", no self-reference — add a
/// condition here the day a fixture needs one.
fn package_entry(pkg_json: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(pkg_json).ok()?;
    fn pick(v: &serde_json::Value) -> Option<String> {
        match v {
            serde_json::Value::String(s) => Some(s.clone()),
            serde_json::Value::Object(m) => ["import", "default"]
                .iter()
                .find_map(|c| m.get(*c).and_then(pick)),
            _ => None,
        }
    }
    if let Some(exports) = v.get("exports") {
        let root = match exports {
            serde_json::Value::Object(m) if m.contains_key(".") => &m["."],
            other => other,
        };
        if let Some(p) = pick(root) {
            return Some(p);
        }
    }
    v.get("main").and_then(|m| m.as_str()).map(str::to_string)
}

/// Resolve a specifier from the importer's directory to a canonical path.
pub fn resolve_specifier(
    spec: &str,
    importer_dir: &Path,
    cfg: &ResolveConfig,
) -> Result<PathBuf, String> {
    let try_forms = |base: PathBuf| -> Option<PathBuf> {
        let text = base.display().to_string();
        let mut cands = Vec::with_capacity(8);
        // TS under nodenext writes the *emitted* extension: "./util.js"
        // means util.ts on disk. Try the source form first, then the
        // literal path, then the extensionless candidates.
        for (js, ts) in [(".js", ".ts"), (".mjs", ".mts"), (".cjs", ".cts")] {
            if let Some(stem) = text.strip_suffix(js) {
                cands.push(PathBuf::from(format!("{stem}{ts}")));
                if js == ".js" {
                    cands.push(PathBuf::from(format!("{stem}.tsx")));
                }
            }
        }
        cands.push(base.clone());
        for ext in ["ts", "tsx", "mts", "cts"] {
            cands.push(PathBuf::from(format!("{text}.{ext}")));
        }
        cands.push(base.join("index.ts"));
        cands.push(base.join("index.tsx"));
        cands
            .into_iter()
            .find(|c| c.is_file())
            .and_then(|c| c.canonicalize().ok())
    };
    if spec.starts_with("./") || spec.starts_with("../") {
        try_forms(importer_dir.join(spec))
            .ok_or_else(|| format!("cannot resolve '{spec}' from {}", importer_dir.display()))
    } else {
        // paths prefix mappings first: "@app/x" -> "<root>/./src/x"
        for (prefix, target) in &cfg.paths {
            if let Some(rest) = spec.strip_prefix(prefix.as_str()) {
                if let Some(p) = try_forms(cfg.root.join(target).join(rest)) {
                    return Ok(p);
                }
            }
        }
        for dir in &cfg.module_dirs {
            if let Some(p) = try_forms(cfg.root.join(dir).join(spec)) {
                return Ok(p);
            }
        }
        if let Some(p) = resolve_node_modules(spec, importer_dir, &try_forms) {
            return Ok(p);
        }
        Err(format!(
            "cannot resolve bare specifier '{spec}' (searched {:?} under {}, \
             then node_modules)",
            cfg.module_dirs,
            cfg.root.display()
        ))
    }
}

/// Walk node_modules upward from the importer, as Node does. The package
/// entry comes from package.json ("exports"/"main"); absent or unresolvable,
/// fall back to the package root's index candidates.
fn resolve_node_modules(
    spec: &str,
    importer_dir: &Path,
    try_forms: &dyn Fn(PathBuf) -> Option<PathBuf>,
) -> Option<PathBuf> {
    // "@scope/pkg/sub/path" -> package "@scope/pkg", subpath "sub/path"
    let parts: Vec<&str> = spec.splitn(if spec.starts_with('@') { 3 } else { 2 }, '/').collect();
    let (pkg, subpath) = if spec.starts_with('@') && parts.len() >= 2 {
        (format!("{}/{}", parts[0], parts[1]), parts.get(2).copied())
    } else {
        (parts[0].to_string(), parts.get(1).copied())
    };
    let mut dir = Some(importer_dir);
    while let Some(d) = dir {
        let root = d.join("node_modules").join(&pkg);
        if root.is_dir() {
            // a deep import addresses a file inside the package directly
            if let Some(sub) = subpath {
                return try_forms(root.join(sub));
            }
            let entry = std::fs::read_to_string(root.join("package.json"))
                .ok()
                .and_then(|t| package_entry(&t));
            if let Some(e) = entry {
                if let Some(p) = try_forms(root.join(&e)) {
                    return Some(p);
                }
            }
            return try_forms(root);
        }
        dir = d.parent();
    }
    None
}

/// Cheap raw-text check: does this source possibly use module syntax?
/// (word-boundary scan; false positives are fine — the parse decides.)
pub fn looks_like_module(source: &str) -> bool {
    for word in ["import", "export"] {
        let mut from = 0;
        while let Some(i) = source[from..].find(word) {
            let at = from + i;
            let before_ok = at == 0
                || !source.as_bytes()[at - 1].is_ascii_alphanumeric()
                    && source.as_bytes()[at - 1] != b'_';
            let after = at + word.len();
            let after_ok = after >= source.len()
                || !source.as_bytes()[after].is_ascii_alphanumeric()
                    && source.as_bytes()[after] != b'_';
            if before_ok && after_ok {
                return true;
            }
            from = after;
        }
    }
    false
}

struct Discovered {
    source: String,
    /// specifier -> canonical dep id, in first-appearance order
    deps: Vec<(String, Arc<str>)>,
    exports: Vec<ExportMeta>,
    /// local import bindings: name -> (dep id, imported name | None=ns)
    imports: Vec<(String, Arc<str>, Option<Arc<str>>)>,
}

/// The module global key for a canonical id.
pub fn module_key(id: &str) -> Arc<str> {
    Arc::from(format!("\0mod:{id}"))
}

/// Compile the whole graph reachable from `entry`.
pub fn compile_graph(entry: &Path) -> Result<Program, ModuleError> {
    let cfg = ResolveConfig::discover(entry);
    let entry_canon = entry.canonicalize().map_err(|e| ModuleError {
        path: entry.display().to_string(),
        msg: format!("cannot read entry: {e}"),
        span_start: 0,
    })?;

    // ---- Phase A: BFS discovery ----
    let mut order: Vec<Arc<str>> = Vec::new();
    let mut found: HashMap<Arc<str>, Discovered> = HashMap::new();
    let mut queue: Vec<PathBuf> = vec![entry_canon.clone()];
    while let Some(path) = queue.pop() {
        let id: Arc<str> = Arc::from(path.display().to_string());
        if found.contains_key(&id) {
            continue;
        }
        let d = discover_one(&path, &cfg)?;
        for (_, dep_id) in &d.deps {
            if !found.contains_key(dep_id) {
                queue.push(PathBuf::from(dep_id.as_ref()));
            }
        }
        order.push(id.clone());
        found.insert(id, d);
    }

    // ---- effective export tables: re-export mutability propagates from
    // the origin (a shared cell stays a cell through any chain), and
    // `export * from` expands to the dep's effective set. Iterate to a
    // fixpoint (cycles converge: entries only ever get added) ----
    let mut mutability: HashMap<Arc<str>, HashMap<Arc<str>, bool>> = found
        .iter()
        .map(|(id, d)| {
            (
                id.clone(),
                d.exports
                    .iter()
                    .filter(|e| e.from.is_none())
                    .map(|e| (e.name.clone(), e.mutable))
                    .collect(),
            )
        })
        .collect();
    for _ in 0..=found.len() {
        let snapshot = mutability.clone();
        let mut changed = false;
        for (id, d) in &found {
            for e in &d.exports {
                let Some((dep, re)) = &e.from else { continue };
                let dep_tab = snapshot.get(dep);
                match re {
                    ReExport::Named(orig) => {
                        if let Some(&m) = dep_tab.and_then(|t| t.get(orig)) {
                            let tab = mutability.get_mut(id).unwrap();
                            if tab.insert(e.name.clone(), m) != Some(m) {
                                changed = true;
                            }
                        }
                    }
                    // opaque: the ns object itself, never a cell
                    ReExport::Namespace => {
                        let tab = mutability.get_mut(id).unwrap();
                        if tab.insert(e.name.clone(), false) != Some(false) {
                            changed = true;
                        }
                    }
                    ReExport::Star => {
                        if let Some(t) = dep_tab {
                            let entries: Vec<_> = t
                                .iter()
                                .filter(|(k, _)| k.as_ref() != "default")
                                .map(|(k, v)| (k.clone(), *v))
                                .collect();
                            let tab = mutability.get_mut(id).unwrap();
                            for (k, v) in entries {
                                if !tab.contains_key(&k) {
                                    tab.insert(k, v);
                                    changed = true;
                                }
                            }
                        }
                    }
                }
            }
        }
        if !changed {
            break;
        }
    }

    // ---- Phase B: per-file emission, parallel across files (each
    // compile_module is independent; shapes/ids are process-global
    // behind their own locks) ----
    let entry_id: Arc<str> = Arc::from(entry_canon.display().to_string());
    let entry_idx = order.iter().position(|id| *id == entry_id).unwrap_or(0);
    let jobs: Vec<(usize, Arc<str>)> =
        order.iter().cloned().enumerate().collect();
    let results: Vec<Result<Module, ModuleError>> = std::thread::scope(|scope| {
        let handles: Vec<_> = jobs
            .iter()
            .map(|(_, id)| {
                let d = &found[id];
                let mutability = &mutability;
                let id = id.clone();
                scope.spawn(move || compile_one(&id, d, mutability))
            })
            .collect();
        handles.into_iter().map(|h| h.join().expect("compile thread")).collect()
    });
    let mut modules = Vec::with_capacity(order.len());
    for r in results {
        modules.push(r?);
    }
    return Ok(Program { modules, entry: entry_idx });
}

fn compile_one(
    id: &Arc<str>,
    d: &Discovered,
    mutability: &HashMap<Arc<str>, HashMap<Arc<str>, bool>>,
) -> Result<Module, ModuleError> {
    {
        let mut imports = HashMap::new();
        for (local, dep_id, imported) in &d.imports {
            let kind = match imported {
                None => tsc_parser::ImportKind::Namespace(
                    mutability.get(dep_id).cloned().unwrap_or_default(),
                ),
                Some(n) => {
                    let mutable = mutability
                        .get(dep_id)
                        .and_then(|m| m.get(n))
                        .copied()
                        .unwrap_or(false);
                    if mutable {
                        tsc_parser::ImportKind::Cell(n.clone())
                    } else {
                        tsc_parser::ImportKind::Value(n.clone())
                    }
                }
            };
            imports.insert(
                local.clone(),
                tsc_parser::ImportBinding { dep_key: module_key(dep_id), kind },
            );
        }
        let exports = d
            .exports
            .iter()
            .filter(|e| e.from.is_none())
            .filter_map(|e| {
                e.binding
                    .as_ref()
                    .map(|b| (b.to_string(), (e.name.clone(), e.mutable)))
            })
            .collect();
        let ctx = tsc_parser::ModuleCtx { key: module_key(id), imports, exports };
        let chunk = tsc_parser::compile_module(&d.source, id, ctx).map_err(|e| ModuleError {
            path: id.to_string(),
            msg: e.msg,
            span_start: e.span_start,
        })?;
        Ok(Module {
            id: id.clone(),
            source_name: id.to_string(),
            main: chunk.main,
            deps: d.deps.iter().map(|(_, i)| i.clone()).collect(),
            exports: d.exports.clone(),
        })
    }
}

fn discover_one(path: &Path, cfg: &ResolveConfig) -> Result<Discovered, ModuleError> {
    let id: Arc<str> = Arc::from(path.display().to_string());
    let err = |msg: String, span: u32| ModuleError {
        path: id.to_string(),
        msg,
        span_start: span,
    };
    let source = std::fs::read_to_string(path)
        .map_err(|e| err(format!("cannot read module: {e}"), 0))?;
    let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let allocator = tsc_ast::oxc_allocator::Allocator::default();
    let program = tsc_ast::parse(&allocator, &source)
        .map_err(|e| err(format!("parse error: {}", e[0].msg), e[0].span_start))?;

    // module-local mutability (which top-level bindings are reassigned)
    let (_captured, mutated, _caps) = tsc_parser::resolve::Resolver::run(&program);

    // top-level binding name -> declaration span (for `export {x}` forms)
    let mut decl_spans: HashMap<String, u32> = HashMap::new();
    for stmt in &program.body {
        let d = match stmt {
            Statement::VariableDeclaration(v) => Some(&**v),
            Statement::ExportNamedDeclaration(e) => match &e.declaration {
                Some(Declaration::VariableDeclaration(v)) => Some(&**v),
                _ => None,
            },
            _ => None,
        };
        if let Some(v) = d {
            for decl in &v.declarations {
                if let BindingPattern::BindingIdentifier(b) = &decl.id {
                    decl_spans.insert(b.name.to_string(), b.span.start);
                }
            }
        }
        let f = match stmt {
            Statement::FunctionDeclaration(f) => Some(&**f),
            Statement::ExportNamedDeclaration(e) => match &e.declaration {
                Some(Declaration::FunctionDeclaration(f)) => Some(&**f),
                _ => None,
            },
            _ => None,
        };
        if let Some(f) = f {
            if let Some(idn) = &f.id {
                decl_spans.insert(idn.name.to_string(), idn.span.start);
            }
        }
    }

    let mut deps: Vec<(String, Arc<str>)> = Vec::new();
    let mut exports: Vec<ExportMeta> = Vec::new();
    let mut imports = Vec::new();
    let add_dep = |spec: &str, span: u32,
                       deps: &mut Vec<(String, Arc<str>)>|
     -> Result<Arc<str>, ModuleError> {
        if let Some((_, id)) = deps.iter().find(|(s, _)| s == spec) {
            return Ok(id.clone());
        }
        let p = resolve_specifier(spec, &dir, cfg)
            .map_err(|m| err(m, span))?;
        let dep: Arc<str> = Arc::from(p.display().to_string());
        deps.push((spec.to_string(), dep.clone()));
        Ok(dep)
    };

    // binding spans of top-level declarations (for export mutability)
    for stmt in &program.body {
        match stmt {
            Statement::ImportDeclaration(i) => {
                if i.import_kind == ImportOrExportKind::Type {
                    continue;
                }
                let dep = add_dep(&i.source.value, i.span.start, &mut deps)?;
                if let Some(specs) = &i.specifiers {
                    for sp in specs {
                        match sp {
                            ImportDeclarationSpecifier::ImportSpecifier(s) => {
                                if s.import_kind == ImportOrExportKind::Type {
                                    continue;
                                }
                                imports.push((
                                    s.local.name.to_string(),
                                    dep.clone(),
                                    Some(Arc::from(s.imported.name().as_str())),
                                ));
                            }
                            ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                                imports.push((
                                    s.local.name.to_string(),
                                    dep.clone(),
                                    Some(Arc::from("default")),
                                ));
                            }
                            ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                                imports.push((s.local.name.to_string(), dep.clone(), None));
                            }
                        }
                    }
                }
            }
            Statement::ExportNamedDeclaration(e) => {
                if e.export_kind == ImportOrExportKind::Type {
                    continue;
                }
                if let Some(src) = &e.source {
                    let dep = add_dep(&src.value, e.span.start, &mut deps)?;
                    for sp in &e.specifiers {
                        exports.push(ExportMeta {
                            name: Arc::from(sp.exported.name().as_str()),
                            mutable: false, // re-export copies the field
                            binding: None,
                            from: Some((
                                dep.clone(),
                                ReExport::Named(Arc::from(sp.local.name().as_str())),
                            )),
                        });
                    }
                    continue;
                }
                if let Some(d) = &e.declaration {
                    match d {
                        Declaration::VariableDeclaration(v) => {
                            for decl in &v.declarations {
                                if let BindingPattern::BindingIdentifier(b) = &decl.id
                                {
                                    exports.push(ExportMeta {
                                        name: Arc::from(b.name.as_str()),
                                        mutable: mutated.contains(&b.span.start),
                                        binding: Some(Arc::from(b.name.as_str())),
                                        from: None,
                                    });
                                }
                            }
                        }
                        Declaration::FunctionDeclaration(f) => {
                            if let Some(idn) = &f.id {
                                exports.push(ExportMeta {
                                    name: Arc::from(idn.name.as_str()),
                                    // fn decls are cell-bound (hoist store)
                                    mutable: true,
                                    binding: Some(Arc::from(idn.name.as_str())),
                                    from: None,
                                });
                            }
                        }
                        _ => {} // type-only
                    }
                }
                for sp in &e.specifiers {
                    let local = sp.local.name();
                    let mutable = decl_spans
                        .get(local.as_str())
                        .is_some_and(|sp| mutated.contains(sp));
                    exports.push(ExportMeta {
                        name: Arc::from(sp.exported.name().as_str()),
                        mutable,
                        binding: Some(Arc::from(local.as_str())),
                        from: None,
                    });
                }
            }
            Statement::ExportAllDeclaration(e) => {
                if e.export_kind == ImportOrExportKind::Type {
                    continue;
                }
                let dep = add_dep(&e.source.value, e.span.start, &mut deps)?;
                // `export * as ns from` is one immutable name bound to the
                // dep's namespace, not a splat of the dep's export set
                let (name, re) = match &e.exported {
                    Some(alias) => {
                        (Arc::from(alias.name().as_str()), ReExport::Namespace)
                    }
                    None => (Arc::from("*"), ReExport::Star),
                };
                exports.push(ExportMeta {
                    name,
                    mutable: false,
                    binding: None,
                    from: Some((dep, re)),
                });
            }
            _ => {}
        }
    }
    Ok(Discovered { source, deps, exports, imports })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_at(root: &Path) -> ResolveConfig {
        ResolveConfig { root: root.to_path_buf(), module_dirs: vec![], paths: vec![] }
    }

    #[test]
    fn js_specifier_resolves_to_ts_source() {
        let dir = std::env::temp_dir().join("tscore_resolve_js");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("util.ts"), "export const x = 1;\n").unwrap();
        let cfg = cfg_at(&dir);
        // "./util.js" is what tsc-checked ESM writes; util.ts is on disk
        let got = resolve_specifier("./util.js", &dir, &cfg).unwrap();
        assert_eq!(got, dir.join("util.ts").canonicalize().unwrap());
        // the plain and extensionless forms still work
        assert!(resolve_specifier("./util.ts", &dir, &cfg).is_ok());
        assert!(resolve_specifier("./util", &dir, &cfg).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
