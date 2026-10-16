//! Keep generated documentation in sync with the code.
//! Regenerate with `UPDATE_FIXTURES=1 cargo test --test docs`.

use std::fmt::Write as _;

use anipv::tui::ui::HELP;

fn keybindings_md() -> String {
    let mut s = String::from(
        "# Key bindings\n\n<!-- Generated from `HELP` in src/tui/ui.rs by tests/docs.rs; do not edit by hand. -->\n\nPress `?` in the TUI to see these at any time.\n",
    );
    for (section, keys) in HELP {
        let _ = write!(s, "\n## {section}\n\n| Key | Action |\n| --- | --- |\n");
        for (k, d) in *keys {
            let key = k.replace('|', "\\|");
            let _ = writeln!(s, "| `{key}` | {d} |");
        }
    }
    s
}

#[test]
fn keybindings_doc_is_current() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/keybindings.md");
    let want = keybindings_md();
    if std::env::var_os("UPDATE_FIXTURES").is_some() {
        std::fs::write(path, &want).unwrap();
        return;
    }
    let have = std::fs::read_to_string(path).unwrap_or_default();
    assert_eq!(have, want, "docs/keybindings.md is stale; run UPDATE_FIXTURES=1 cargo test --test docs");
}

/// The `?` popup is 64 columns wide: 2 for its border and 2 for the
/// indent, 12 for the key, so a description has 48 before it is cut off.
#[test]
fn help_lines_fit_the_popup() {
    use unicode_width::UnicodeWidthStr;
    for (section, keys) in HELP {
        for (key, desc) in *keys {
            let (k, d) = (key.width(), desc.width());
            assert!(k <= 12, "{section}: key {key:?} is {k} columns wide (12 fit)");
            assert!(d <= 48, "{section}: {key:?} is {d} columns wide (48 fit): {desc}");
        }
    }
}

#[test]
fn config_doc_mentions_every_key() {
    let doc = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/config.md")).unwrap();
    let cfg = toml::to_string(&anipv::config::Config::default()).unwrap();
    for line in cfg.lines().filter(|l| l.contains(" = ")) {
        let key = line.split(" = ").next().unwrap().trim();
        assert!(doc.contains(&format!("`{key}`")), "docs/config.md does not document `{key}`");
    }
    for key in ["device", "events_dir", "roots"] {
        assert!(doc.contains(&format!("`{key}`")), "docs/config.md does not document `{key}`");
    }
}

/// docs/anilist.md lists the rules for talking to AniList, each with the
/// tests that hold it: every rule has one, and every test named exists.
#[test]
fn anilist_rules_name_real_tests() {
    let root = env!("CARGO_MANIFEST_DIR");
    let doc = std::fs::read_to_string(format!("{root}/docs/anilist.md")).unwrap();
    let mut code = String::new();
    let mut dirs = vec![std::path::PathBuf::from(format!("{root}/src")), format!("{root}/tests").into()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                code.push_str(&std::fs::read_to_string(path).unwrap());
            }
        }
    }
    let is_rule = |l: &&str| l.strip_prefix("| R").is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()));
    let rules: Vec<&str> = doc.lines().filter(is_rule).collect();
    assert!(rules.len() >= 18, "docs/anilist.md: only {n} rules found", n = rules.len());
    for (i, row) in rules.iter().enumerate() {
        let id = format!("R{n}", n = i + 1);
        assert!(row.starts_with(&format!("| {id} |")), "docs/anilist.md: expected {id} next: {row}");
        let tests: Vec<&str> =
            row.trim_end_matches('|').rsplit('|').next().unwrap().split('`').skip(1).step_by(2).collect();
        assert!(!tests.is_empty(), "docs/anilist.md: {id} names no test");
        for test in tests {
            let defined = ["#[test]\n    fn", "#[test]\nfn"].iter().any(|t| code.contains(&format!("{t} {test}()")));
            assert!(defined, "docs/anilist.md: {id} names `{test}`, which isn't a test");
        }
    }
}
