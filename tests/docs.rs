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
