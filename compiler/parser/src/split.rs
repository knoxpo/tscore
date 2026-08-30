//! Top-level function splitter for the parallel frontend (M6e).
//!
//! Scans raw source and finds top-level `function` / `async function`
//! declarations by brace matching, so their bodies can be (a) hollowed to
//! spaces before the main oxc parse — length-preserving, every span still
//! matches the original file — and (b) parsed + capture-resolved in
//! parallel worker threads.
//!
//! Correctness beats coverage: any construct the scanner is not sure
//! about returns None and the caller falls back to the serial pipeline.
//! Bails on: top-level `let`/`const`/`var`/`class` (their bindings could
//! be captured by functions, which needs the full-file resolver walk),
//! `import`/`export` (subset errors — let the serial path report them),
//! and unbalanced nesting. The subset has no regex literals, so `/` never
//! starts a string-like token.

/// One top-level function item. All offsets are byte positions in the
/// original source; `body` is the range INSIDE the braces.
pub struct FnItem {
    pub start: u32,
    pub end: u32,
    pub body_start: u32,
    pub body_end: u32,
}

const BAIL_WORDS: [&str; 6] = ["let", "const", "var", "class", "import", "export"];

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Try to split `src` into top-level function items. None = use the
/// serial pipeline.
pub fn split_top_level(src: &str) -> Option<Vec<FnItem>> {
    let b = src.as_bytes();
    let n = b.len();
    let mut items = Vec::new();
    let mut i = 0usize;
    // template nesting: each entry is the brace depth at which a `${`
    // opened inside the template at that level
    let mut depth: i64 = 0;

    // scan one code-region token stream; returns after consuming strings,
    // comments and templates transparently
    macro_rules! skip_ws_comments {
        () => {
            loop {
                while i < n && b[i].is_ascii_whitespace() {
                    i += 1;
                }
                if i + 1 < n && b[i] == b'/' && b[i + 1] == b'/' {
                    while i < n && b[i] != b'\n' {
                        i += 1;
                    }
                } else if i + 1 < n && b[i] == b'/' && b[i + 1] == b'*' {
                    i += 2;
                    while i + 1 < n && !(b[i] == b'*' && b[i + 1] == b'/') {
                        i += 1;
                    }
                    i = (i + 2).min(n);
                } else {
                    break;
                }
            }
        };
    }

    // consume a string/template starting at i; returns None on scanner
    // confusion (unterminated); templates recurse into ${ } code
    fn skip_string(b: &[u8], mut i: usize) -> Option<usize> {
        let n = b.len();
        let q = b[i];
        i += 1;
        if q == b'`' {
            while i < n {
                match b[i] {
                    b'\\' => i += 2,
                    b'`' => return Some(i + 1),
                    b'$' if i + 1 < n && b[i + 1] == b'{' => {
                        // nested code until the matching close brace
                        i += 2;
                        let mut d = 1i64;
                        while i < n && d > 0 {
                            match b[i] {
                                b'{' => {
                                    d += 1;
                                    i += 1;
                                }
                                b'}' => {
                                    d -= 1;
                                    i += 1;
                                }
                                b'\'' | b'"' | b'`' => i = skip_string(b, i)?,
                                b'/' if i + 1 < n && b[i + 1] == b'/' => {
                                    while i < n && b[i] != b'\n' {
                                        i += 1;
                                    }
                                }
                                b'/' if i + 1 < n && b[i + 1] == b'*' => {
                                    i += 2;
                                    while i + 1 < n && !(b[i] == b'*' && b[i + 1] == b'/') {
                                        i += 1;
                                    }
                                    i += 2;
                                }
                                _ => i += 1,
                            }
                        }
                        if d != 0 {
                            return None;
                        }
                    }
                    _ => i += 1,
                }
            }
            None
        } else {
            while i < n {
                match b[i] {
                    b'\\' => i += 2,
                    c if c == q => return Some(i + 1),
                    b'\n' => return None,
                    _ => i += 1,
                }
            }
            None
        }
    }

    while i < n {
        skip_ws_comments!();
        if i >= n {
            break;
        }
        let c = b[i];
        match c {
            b'\'' | b'"' | b'`' => {
                i = skip_string(b, i)?;
            }
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                depth -= 1;
                if depth < 0 {
                    return None;
                }
                i += 1;
            }
            c if is_word_byte(c) => {
                let ws = i;
                while i < n && is_word_byte(b[i]) {
                    i += 1;
                }
                let word = &src[ws..i];
                let boundary = ws == 0 || !is_word_byte(b[ws - 1]);
                if depth == 0 && boundary {
                    if BAIL_WORDS.contains(&word) {
                        return None;
                    }
                    let is_fn_kw = word == "function"
                        || (word == "async" && {
                            let mut j = i;
                            while j < n && b[j].is_ascii_whitespace() {
                                j += 1;
                            }
                            src[j..].starts_with("function")
                                && !is_word_byte(*b.get(j + 8).unwrap_or(&b' '))
                        });
                    if is_fn_kw {
                        let start = ws;
                        // skip to the body-open brace at paren depth 0
                        let mut pd = 0i64;
                        loop {
                            skip_ws_comments!();
                            if i >= n {
                                return None;
                            }
                            match b[i] {
                                b'\'' | b'"' | b'`' => i = skip_string(b, i)?,
                                b'(' | b'[' => {
                                    pd += 1;
                                    i += 1;
                                }
                                b')' | b']' => {
                                    pd -= 1;
                                    i += 1;
                                }
                                b'{' if pd == 0 => break,
                                _ => i += 1,
                            }
                        }
                        let body_start = i;
                        i += 1;
                        let mut d = 1i64;
                        while i < n && d > 0 {
                            skip_ws_comments!();
                            if i >= n {
                                break;
                            }
                            match b[i] {
                                b'\'' | b'"' | b'`' => i = skip_string(b, i)?,
                                b'{' => {
                                    d += 1;
                                    i += 1;
                                }
                                b'}' => {
                                    d -= 1;
                                    i += 1;
                                }
                                _ => i += 1,
                            }
                        }
                        if d != 0 {
                            return None;
                        }
                        items.push(FnItem {
                            start: start as u32,
                            end: i as u32,
                            body_start: body_start as u32,
                            body_end: (i - 1) as u32,
                        });
                    }
                }
            }
            _ => i += 1,
        }
    }
    if depth != 0 {
        return None;
    }
    Some(items)
}

/// Source with every item's body interior blanked to spaces —
/// length-preserving, so all spans in the hollow parse line up with the
/// original file. Newlines are kept so line-based tooling stays sane.
pub fn hollow(src: &str, items: &[FnItem]) -> String {
    let mut out = src.as_bytes().to_vec();
    for it in items {
        for byte in &mut out[it.body_start as usize + 1..it.body_end as usize] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    }
    // scanner guarantees ASCII-safe edits only (spaces over arbitrary
    // bytes keep UTF-8 validity because we never split a code point:
    // multi-byte sequences are fully inside the blanked range or fully
    // outside — blanking every byte of the range keeps it valid)
    String::from_utf8(out).expect("hollowing preserved utf8")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_two_functions() {
        let src = "function a() { return 1; }\nfunction b(x) { if (x) { return { k: 1 }; } }\na(); b(2);";
        let items = split_top_level(src).unwrap();
        assert_eq!(items.len(), 2);
        let h = hollow(src, &items);
        assert_eq!(h.len(), src.len());
        assert!(h.starts_with("function a() {"));
        assert!(h.contains("a(); b(2);"));
        assert!(!h.contains("k: 1"));
    }

    #[test]
    fn bails_on_top_level_const() {
        assert!(split_top_level("const x = 1;\nfunction f() {}").is_none());
    }

    #[test]
    fn const_inside_body_is_fine() {
        let src = "function f() { const x = `a${1 + 2}b`; return x; }";
        let items = split_top_level(src).unwrap();
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn async_functions_and_nested_braces() {
        let src = "async function f() { await g(); }\nfunction g() { let s = \"}{\"; return s; }";
        let items = split_top_level(src).unwrap();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn block_statement_functions_stay_in_rest() {
        let src = "{ function inner() {} }\nfunction top() {}";
        let items = split_top_level(src).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(&src[items[0].start as usize..items[0].end as usize], "function top() {}");
    }
}
