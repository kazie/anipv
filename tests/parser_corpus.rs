//! Golden test: one synthetic file name per naming convention and how it parses.
//!
//! `tests/fixtures/filenames.tsv` columns: name, kind, episode, title, label, key.
//!
//! `label` and `key` (the series key a loose file gets) are *identity*: they are
//! written into the event log, so a change here can orphan recorded history.
//! Treat a diff in those columns as a breaking change.
//! After an intentional parser change, regenerate with
//! `UPDATE_FIXTURES=1 cargo test --test parser_corpus` and review the diff.

use anipv::parse::parse;

const PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/filenames.tsv");

fn row(name: &str) -> String {
    let p = parse(name);
    let ep = match (p.ep, p.ep_end) {
        (Some(a), Some(b)) => format!("{a}-{b}"),
        (Some(a), None) => a.to_string(),
        _ => String::new(),
    };
    let key = anipv::identity::series_name(&p.title, p.season).0;
    let (kind, title, label) = (p.kind.as_str(), &p.title, &p.label);
    format!("{name}\t{kind}\t{ep}\t{title}\t{label}\t{key}")
}

#[test]
fn corpus_matches_golden_file() {
    let text = std::fs::read_to_string(PATH).unwrap();
    let names: Vec<&str> =
        text.lines().filter(|l| !l.starts_with('#')).map(|l| l.split('\t').next().unwrap()).collect();
    let actual: Vec<String> = names.iter().map(|n| row(n)).collect();
    if std::env::var_os("UPDATE_FIXTURES").is_some() {
        let header = text.lines().take_while(|l| l.starts_with('#')).collect::<Vec<_>>().join("\n");
        std::fs::write(PATH, format!("{header}\n{}\n", actual.join("\n"))).unwrap();
        return;
    }
    let expected: Vec<&str> = text.lines().filter(|l| !l.starts_with('#')).collect();
    let mut failures = Vec::new();
    for (e, a) in expected.iter().zip(&actual) {
        if e != a {
            failures.push(format!("  expected: {e}\n  actual:   {a}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} names parse differently:\n{}",
        failures.len(),
        names.len(),
        failures.join("\n")
    );
}

#[test]
fn parser_never_panics_on_corpus_variants() {
    let text = std::fs::read_to_string(PATH).unwrap();
    for name in text.lines().filter(|l| !l.starts_with('#')).map(|l| l.split('\t').next().unwrap()) {
        // Mangled variants: truncated, separators swapped, extension dropped.
        let half: String = name.chars().take(name.chars().count() / 2).collect();
        for v in [half.as_str(), &name.replace(' ', "."), &name.replace(' ', "_"), name.trim_end_matches(".mkv")] {
            let _ = parse(v);
        }
    }
}

proptest::proptest! {
    /// `parse` and `classify` never panic, whatever the name (multi-byte text,
    /// stray brackets, dots and digits included).
    #[test]
    fn parse_and_classify_never_panic(
        name in r"(?s).{0,60}|[\[\]() ._\-0-9a-zA-Z.xXsSeEvバ]{0,40}(\.mkv)?",
        dir in r"[\[\]() ._\-0-9a-zA-Zバ]{0,20}",
    ) {
        let _ = parse(&name);
        let _ = anipv::parse::clean_dir_name(&name);
        let cfg = anipv::config::Config::default();
        for kind in [anipv::config::RootKind::Archive, anipv::config::RootKind::Ongoing] {
            let _ = anipv::index::classify::classify(&cfg, kind, std::path::Path::new(&format!("{dir}/{name}")));
            let _ = anipv::index::classify::classify(&cfg, kind, std::path::Path::new(&name));
        }
    }
}
