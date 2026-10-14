//! Code style that no compiler or clippy lint checks.
//!
//! Format strings name every argument: `format!("{title} {n}/{total}",
//! n = pos + 1, total = stops.len())`, never `format!("{} {}/{}", …)`.
//! Clippy's `uninlined_format_args` covers plain variables (`{x}`); this
//! covers the expressions it can't (`{}` with `pos + 1`, `{:?}`, `{0}`).

use std::path::{Path, PathBuf};

/// Format macros, with the index of the format string among their arguments.
const MACROS: &[(&str, usize)] = &[
    ("format", 0),
    ("format_args", 0),
    ("print", 0),
    ("println", 0),
    ("eprint", 0),
    ("eprintln", 0),
    ("panic", 0),
    ("unreachable", 0),
    ("todo", 0),
    ("unimplemented", 0),
    ("bail", 0),
    ("anyhow", 0),
    ("write", 1),
    ("writeln", 1),
    ("assert", 1),
    ("debug_assert", 1),
    ("ensure", 1),
    ("assert_eq", 2),
    ("assert_ne", 2),
    ("debug_assert_eq", 2),
    ("debug_assert_ne", 2),
];

/// The end (exclusive) of the string literal starting at `i` (`"…"`, `r#"…"#`,
/// `b"…"`), if one starts there.
fn string_end(src: &[u8], start: usize) -> Option<usize> {
    let ident_before = start > 0 && (src[start - 1].is_ascii_alphanumeric() || src[start - 1] == b'_');
    let mut at = start;
    if src.get(at) == Some(&b'b') && !ident_before {
        at += 1;
    }
    if src.get(at) == Some(&b'r') && !ident_before {
        let mut quote = at + 1;
        while src.get(quote) == Some(&b'#') {
            quote += 1;
        }
        if src.get(quote) == Some(&b'"') {
            let hashes = quote - at - 1;
            let closes = |pos: usize| src[pos + 1..].iter().take(hashes).filter(|&&c| c == b'#').count() == hashes;
            let end = (quote + 1..src.len()).find(|&pos| src[pos] == b'"' && closes(pos))?;
            return Some(end + 1 + hashes);
        }
    }
    if src.get(at) != Some(&b'"') || (at == start && ident_before && src[start] != b'"') {
        return None;
    }
    let mut pos = at + 1;
    while pos < src.len() {
        match src[pos] {
            b'\\' => pos += 2,
            b'"' => return Some(pos + 1),
            _ => pos += 1,
        }
    }
    None
}

/// The top-level arguments of the macro call whose delimiter opens just
/// before `i`, as byte ranges.
fn arguments(s: &[u8], mut i: usize) -> Option<Vec<(usize, usize)>> {
    let (mut depth, mut start, mut out) = (0_usize, i, Vec::new());
    while i < s.len() {
        if s[i..].starts_with(b"//") {
            i += s[i..].iter().position(|&c| c == b'\n')?;
            continue;
        }
        if s[i..].starts_with(b"/*") {
            i += s[i..].windows(2).position(|w| w == b"*/")? + 2;
            continue;
        }
        if let Some(end) = string_end(s, i) {
            i = end;
            continue;
        }
        match s[i] {
            // A char literal ('x', '\n', '\u{..}'); otherwise a lifetime.
            b'\'' => {
                let close = s[i + 1..].iter().take(12).position(|&c| c == b'\'');
                i += match close {
                    Some(n) if n == 1 || s[i + 1] == b'\\' => n + 2,
                    _ => 1,
                };
                continue;
            }
            b'(' | b'[' | b'{' => depth += 1,
            b')' | b']' | b'}' if depth == 0 => {
                out.push((start, i));
                return Some(out);
            }
            b')' | b']' | b'}' => depth -= 1,
            b',' if depth == 0 => {
                out.push((start, i));
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The placeholders of a format string that name no argument (`{}`, `{:?}`,
/// `{0}`); `{{`, `}}` and `\u{…}` escapes are not placeholders.
fn positional(fmt: &str) -> Vec<String> {
    let b = fmt.as_bytes();
    let (mut out, mut i) = (Vec::new(), 0);
    while i < b.len() {
        if b[i..].starts_with(b"{{") || b[i..].starts_with(b"}}") {
            i += 2;
        } else if b[i..].starts_with(b"\\u{") {
            i += b[i..].iter().position(|&c| c == b'}').map_or(b.len(), |n| n + 1);
        } else if b[i] == b'{' {
            let Some(len) = b[i..].iter().position(|&c| c == b'}') else { break };
            let inner = &fmt[i + 1..i + len];
            let arg = inner.split(':').next().unwrap_or("").trim();
            if arg.is_empty() || arg.bytes().all(|c| c.is_ascii_digit()) {
                out.push(format!("{{{inner}}}"));
            }
            i += len + 1;
        } else {
            i += 1;
        }
    }
    out
}

/// Every format string in `src` with positional placeholders: `line: macro!: placeholders`.
fn offenders(src: &str) -> Vec<String> {
    let s = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let rest = &src[i..];
        let found = MACROS.iter().find(|(name, _)| {
            let ident_before = i > 0 && (s[i - 1].is_ascii_alphanumeric() || s[i - 1] == b'_');
            !ident_before && rest.starts_with(name) && rest[name.len()..].starts_with('!')
        });
        let Some(&(name, at)) = found else {
            // Skip comments and literals, so a macro name inside one isn't taken for a call.
            if rest.starts_with("//") {
                i += rest.find('\n').unwrap_or(rest.len());
            } else if let Some(end) = string_end(s, i) {
                i = end;
            } else {
                i += rest.chars().next().map_or(1, char::len_utf8);
            }
            continue;
        };
        let open = i + name.len() + 1 + rest[name.len() + 1..].find(['(', '[', '{']).unwrap_or(0);
        if let Some(args) = arguments(s, open + 1)
            && let Some(&(a, b)) = args.get(at)
        {
            let lit = src[a..b].trim();
            if let Some(end) = string_end(lit.as_bytes(), 0)
                && end == lit.len()
            {
                let bad = positional(lit);
                if !bad.is_empty() {
                    let line = src[..i].matches('\n').count() + 1;
                    out.push(format!("{line}: {name}!: {bad}", bad = bad.join(" ")));
                }
            }
        }
        i = open + 1;
    }
    out
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|x| x == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn format_strings_name_their_arguments() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for dir in ["src", "tests", "examples"] {
        rust_files(&root.join(dir), &mut files);
    }
    files.sort();
    let mut bad = Vec::new();
    for path in &files {
        let src = std::fs::read_to_string(path).unwrap_or_default();
        let rel = path.strip_prefix(root).unwrap_or(path).display().to_string();
        bad.extend(offenders(&src).into_iter().map(|o| format!("{rel}:{o}")));
    }
    assert!(
        bad.is_empty(),
        "name every format argument (`{{n}}` with `n = pos + 1`), not `{{}}`:\n{list}",
        list = bad.join("\n")
    );
}

/// The check itself: what it flags, and what it lets through.
#[test]
fn positional_placeholders_are_found() {
    let flagged = |code: &str| offenders(code).len();
    assert_eq!(flagged(r#"let s = format!("{} items", v.len());"#), 1);
    assert_eq!(flagged(r#"writeln!(out, "{:?} {0}", a)?;"#), 1);
    assert_eq!(flagged(r#"assert_eq!(a, b, "got {}", c);"#), 1);
    assert_eq!(flagged(r#"let s = format!(r"x={:.1}", x);"#), 1);
    assert_eq!(flagged("let s = format!(\n    \"{}\",\n    f(\"(\", ')'),\n);"), 1);
    // Named, inlined, escaped, or not a format string at all.
    assert_eq!(flagged(r#"let s = format!("{n} items", n = v.len());"#), 0);
    assert_eq!(flagged(r#"let s = format!("{x:?} {{}} \u{1}");"#), 0);
    assert_eq!(flagged(r#"assert_eq!(a, "{}");"#), 0);
    assert_eq!(flagged(r#"let re = r"\{[^{}]*\}"; // format!("{}")"#), 0);
    assert_eq!(flagged(r#"let s = my_format!("{}", x);"#), 0);
}
