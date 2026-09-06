//! `runtime.path` — POSIX path manipulation.
//!
//! Pure string work: no syscalls, no I/O pool, every call synchronous.
//! `normalize` is lexical — it never consults the filesystem, so it does
//! not resolve symlinks (that is `fs.realPath`).
//!
//! The operations are plain functions over `&str` and the natives are thin
//! wrappers, so the edge cases are unit-tested without a Realm.

use tsr_memory::Value;
use tsr_realm::{NativeArgs, Realm, RtError};

const SEP: char = '/';

pub fn is_absolute(p: &str) -> bool {
    p.starts_with(SEP)
}

/// Collapse `.`, `..` and duplicate separators, lexically.
///
/// A trailing separator is always dropped (`"a/b/"` -> `"a/b"`), which is
/// where this diverges from Node's `path.normalize`; one spelling per path
/// is worth more here than matching Node exactly.
pub fn normalize(p: &str) -> String {
    let abs = is_absolute(p);
    let mut out: Vec<&str> = Vec::new();
    for part in p.split(SEP) {
        match part {
            "" | "." => {}
            ".." => match out.last() {
                Some(&last) if last != ".." => {
                    out.pop();
                }
                // `..` above the root is the root itself; in a relative
                // path it has to survive.
                _ => {
                    if !abs {
                        out.push("..");
                    }
                }
            },
            other => out.push(other),
        }
    }
    let joined = out.join("/");
    match (abs, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".to_string(),
        (false, false) => joined,
    }
}

pub fn join(parts: &[String]) -> String {
    let joined = parts
        .iter()
        .filter(|p| !p.is_empty())
        .map(|p| p.as_str())
        .collect::<Vec<_>>()
        .join("/");
    if joined.is_empty() {
        ".".to_string()
    } else {
        normalize(&joined)
    }
}

/// POSIX `dirname`: trailing separators are stripped first, a bare name is
/// `"."`, and the parent of a root child is `"/"`.
pub fn dirname(p: &str) -> String {
    let trimmed = p.trim_end_matches(SEP);
    if trimmed.is_empty() {
        // "" -> ".", but "/" and "///" -> "/"
        return if p.is_empty() { ".".into() } else { "/".into() };
    }
    match trimmed.rfind(SEP) {
        None => ".".into(),
        Some(0) => "/".into(),
        Some(i) => trimmed[..i].trim_end_matches(SEP).to_string(),
    }
}

/// POSIX `basename`: trailing separators are stripped first; the root is
/// its own basename.
pub fn basename(p: &str) -> String {
    let trimmed = p.trim_end_matches(SEP);
    if trimmed.is_empty() {
        return if p.is_empty() {
            String::new()
        } else {
            "/".into()
        };
    }
    match trimmed.rfind(SEP) {
        None => trimmed.to_string(),
        Some(i) => trimmed[i + 1..].to_string(),
    }
}

/// The final `.` and everything after it, or `""`. A leading dot is a
/// dotfile, not an extension: `.gitignore` has none (as in POSIX and Node).
pub fn extname(p: &str) -> String {
    let base = basename(p);
    match base.rfind('.') {
        Some(0) | None => String::new(),
        Some(i) => base[i..].to_string(),
    }
}

// ---------------- natives ----------------

fn str_arg(realm: &Realm, args: NativeArgs, i: usize, who: &str) -> Result<String, RtError> {
    match args.get(realm, i).as_str_ref() {
        Some(r) => Ok(realm.heap.str_at(r).to_string()),
        None => Err(RtError::new(format!("{who}: expected a path string"))),
    }
}

pub fn members(realm: &mut Realm) -> Vec<(&'static str, Value)> {
    let join_fn = realm.add_native(|realm, args| {
        // read every argument out of the heap before allocating anything
        let parts: Result<Vec<String>, RtError> = (0..args.len())
            .map(|i| str_arg(realm, args, i, "path.join"))
            .collect();
        let s = join(&parts?);
        Ok(realm.alloc_string(&s))
    });
    let dirname_fn = realm.add_native(|realm, args| {
        let s = dirname(&str_arg(realm, args, 0, "path.dirname")?);
        Ok(realm.alloc_string(&s))
    });
    let basename_fn = realm.add_native(|realm, args| {
        let s = basename(&str_arg(realm, args, 0, "path.basename")?);
        Ok(realm.alloc_string(&s))
    });
    let extname_fn = realm.add_native(|realm, args| {
        let s = extname(&str_arg(realm, args, 0, "path.extname")?);
        Ok(realm.alloc_string(&s))
    });
    let normalize_fn = realm.add_native(|realm, args| {
        let s = normalize(&str_arg(realm, args, 0, "path.normalize")?);
        Ok(realm.alloc_string(&s))
    });
    let is_absolute_fn = realm.add_native(|realm, args| {
        let p = str_arg(realm, args, 0, "path.isAbsolute")?;
        Ok(Value::bool(is_absolute(&p)))
    });
    vec![
        ("join", join_fn),
        ("dirname", dirname_fn),
        ("basename", basename_fn),
        ("extname", extname_fn),
        ("normalize", normalize_fn),
        ("isAbsolute", is_absolute_fn),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j(parts: &[&str]) -> String {
        join(&parts.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn normalize_collapses_dots_and_separators() {
        assert_eq!(normalize("a//b/./c"), "a/b/c");
        assert_eq!(normalize("a/b/../c"), "a/c");
        assert_eq!(normalize("a/b/"), "a/b");
        assert_eq!(normalize("/a/../b"), "/b");
        assert_eq!(normalize("."), ".");
        assert_eq!(normalize(""), ".");
        assert_eq!(normalize("/"), "/");
    }

    /// `..` cannot escape the root, but must survive in a relative path —
    /// dropping it there would silently rewrite `../x` to `x`.
    #[test]
    fn normalize_handles_dotdot_at_the_boundary() {
        assert_eq!(normalize("/../.."), "/");
        assert_eq!(normalize("/a/../../b"), "/b");
        assert_eq!(normalize("a/../../b"), "../b");
        assert_eq!(normalize("../../a"), "../../a");
        assert_eq!(normalize("a/.."), ".");
        assert_eq!(normalize("../a/.."), "..");
    }

    #[test]
    fn join_skips_empties_and_normalizes() {
        assert_eq!(j(&["a", "b"]), "a/b");
        assert_eq!(j(&["a", "", "b"]), "a/b");
        assert_eq!(j(&["a", "/b"]), "a/b");
        assert_eq!(j(&["/a", "b/../c"]), "/a/c");
        assert_eq!(j(&[]), ".");
        assert_eq!(j(&["", ""]), ".");
    }

    #[test]
    fn dirname_follows_posix() {
        assert_eq!(dirname("a"), ".");
        assert_eq!(dirname("a/b"), "a");
        assert_eq!(dirname("/a"), "/");
        assert_eq!(dirname("/a/b"), "/a");
        assert_eq!(dirname("a/b/"), "a");
        assert_eq!(dirname("/"), "/");
        assert_eq!(dirname(""), ".");
        assert_eq!(dirname("a//b"), "a");
    }

    #[test]
    fn basename_follows_posix() {
        assert_eq!(basename("a/b"), "b");
        assert_eq!(basename("a/b/"), "b");
        assert_eq!(basename("a"), "a");
        assert_eq!(basename("/"), "/");
        assert_eq!(basename(""), "");
    }

    /// A leading dot is a dotfile, not an extension.
    #[test]
    fn extname_ignores_dotfiles() {
        assert_eq!(extname("a.txt"), ".txt");
        assert_eq!(extname("a.b.c"), ".c");
        assert_eq!(extname("dir.d/file"), "");
        assert_eq!(extname(".gitignore"), "");
        assert_eq!(extname("index."), ".");
        assert_eq!(extname("a"), "");
    }

    #[test]
    fn is_absolute_is_leading_separator() {
        assert!(is_absolute("/a"));
        assert!(!is_absolute("a"));
        assert!(!is_absolute(""));
    }
}
