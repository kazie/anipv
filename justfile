# https://github.com/casey/just
# Every recipe is plain cargo (or a shell line); `just --list` shows them all.

# Same as `just check`.
default: check

# Format check, lints, tests, docs and cargo-deny (needs cargo-deny).
check: fmt-check clippy test doc deny

# The local version of CI: `check`, then the MSRV build.
ci: check msrv

# Format the code.
fmt:
    cargo fmt --all

# Check formatting without changing anything.
fmt-check:
    cargo fmt --all --check

# Lint, failing on any warning.
clippy:
    cargo clippy --all-targets --locked -- -D warnings

# Run the tests.
test:
    cargo test --locked

# API docs, failing on broken intra-doc links.
doc:
    RUSTDOCFLAGS='-D warnings' cargo doc --no-deps --locked

# License and advisory checks (needs cargo-deny).
deny:
    cargo deny check

# Build with the minimum supported Rust version (rustup toolchain 1.99).
msrv:
    cargo +1.99 check --all-targets --locked

# Apply clippy's suggestions, then format.
fix:
    cargo clippy --fix --allow-dirty --all-targets
    cargo fmt --all

# Print a version's CHANGELOG section, as its GitHub Release will show it.
release-notes version:
    contrib/release-notes.sh {{version}}

# Bump, date the CHANGELOG, commit and tag, e.g. `just release 0.2.0` (runs `ci` first; doesn't push).
release version:
    #!/usr/bin/env bash
    set -euo pipefail
    v={{quote(trim_start_match(version, "v"))}}
    fail() { echo "error: $*" >&2; exit 1; }
    [[ $v =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "version must be X.Y.Z, got '$v'"
    current=$(cargo metadata --no-deps --format-version 1 | jq -r '.packages[] | select(.name == "anipv") | .version')
    [[ $v != "$current" && $(printf '%s\n' "$current" "$v" | sort -V | tail -1) == "$v" ]] \
        || fail "$v isn't newer than the current version $current"
    [[ $(git branch --show-current) == master ]] || fail "not on master"
    [[ -z $(git status --porcelain) ]] || fail "the working tree isn't clean"
    git fetch -q origin master
    [[ $(git rev-parse HEAD) == $(git rev-parse origin/master) ]] || fail "master isn't the same as origin/master"
    ! git rev-parse -q --verify "refs/tags/v$v" >/dev/null || fail "tag v$v already exists"
    unreleased=$(awk 'index($0, "## [") == 1 { if (on) exit; if ($0 == "## [Unreleased]") { on = 1; next } } on && NF' CHANGELOG.md)
    [[ -n $unreleased ]] || fail "CHANGELOG.md has nothing under ## [Unreleased]"

    just ci

    sed -i "0,/^version = \".*\"/s//version = \"$v\"/" Cargo.toml
    cargo update --workspace --offline --quiet
    sed -i "s/^## \[Unreleased\]$/## [Unreleased]\n\n## [$v] - $(date +%F)/" CHANGELOG.md
    git add Cargo.toml Cargo.lock CHANGELOG.md
    git commit -q -m "release: v$v"
    git tag -a "v$v" -m "anipv $v"
    echo "Committed and tagged v$v. Check it (git show), then publish it:"
    echo "    git push origin master v$v"

# Install anipv to ~/.cargo/bin (release build).
install:
    cargo install --path . --locked

# Where `cargo install` (`just install`) puts anipv.
anipv_bin := env("CARGO_INSTALL_ROOT", env("CARGO_HOME", env("HOME") / ".cargo")) / "bin/anipv"

# Run `anipv update` every 15 minutes from a systemd user timer (after `just install`).
install-timer:
    test -x "{{anipv_bin}}" || { echo "{{anipv_bin}} not found: run \`just install\` first"; exit 1; }
    mkdir -p ~/.config/systemd/user
    sed 's|^ExecStart=.*/anipv |ExecStart={{anipv_bin}} |' contrib/systemd/anipv-update.service > ~/.config/systemd/user/anipv-update.service
    cp contrib/systemd/anipv-update.timer ~/.config/systemd/user/
    systemctl --user daemon-reload
    systemctl --user enable --now anipv-update.timer

# Stop and remove the timer `install-timer` set up.
uninstall-timer:
    -systemctl --user disable --now anipv-update.timer
    rm -f ~/.config/systemd/user/anipv-update.service ~/.config/systemd/user/anipv-update.timer
    systemctl --user daemon-reload

# Run anipv with arguments, e.g. `just run next`.
run *args:
    cargo run -- {{args}}

# Write the man page to ./anipv.1.
man:
    cargo run -q -- man > anipv.1

# Print shell completions, e.g. `just completions fish`.
completions shell:
    cargo run -q -- completions {{shell}}

# Garbage-collect stale build artifacts, e.g. `just gc 3` (defaults to 14 days).
gc days="14":
    @if [ -d target ]; then \
        echo "Removing build artifacts older than {{days}} days from target/..."; \
        find target -type f -mtime +{{days}} -exec rm -f {} +; \
        find target -type d -empty -delete; \
        echo "Done."; \
    else \
        echo "No target/ directory found; nothing to clean."; \
    fi

# Clean all build artifacts.
clean:
    cargo clean

# Regenerate README screenshots (SVG) from the demo library.
screenshots:
    cargo build
    cargo run --example screenshots

# Re-record the README GIF and its text, assets/tapes/all.txt (needs vhs and ttyd).
record: _need-vhs demo-home
    PATH="{{justfile_directory()}}/target/debug:$PATH" vhs assets/tapes/all.tape
    @if git ls-files --error-unmatch assets/tapes/all.txt assets/img/demo.gif >/dev/null 2>&1 && git diff --quiet -- assets/tapes/all.txt; then git restore assets/img/demo.gif; echo "assets/tapes/all.txt is unchanged, so the demo is too (only timing or colors differ): kept the GIF you had"; fi

# Fresh demo library in assets/demo-home, as the recordings expect.
demo-home:
    rm -rf assets/demo-home
    cargo run -q --features demo -- demo assets/demo-home

_need-vhs:
    #!/bin/sh
    for tool in vhs ttyd; do
        command -v "$tool" >/dev/null || { echo "error: just record needs $tool on PATH (see https://github.com/charmbracelet/vhs)" >&2; exit 1; }
    done

# Update golden files after intentional changes (review the diff!).
update-fixtures:
    UPDATE_FIXTURES=1 cargo test --test parser_corpus --test docs
    cargo insta test --review

# Try the TUI against a throwaway demo library in target/ (its player is a link there, not a copy).
demo:
    rm -rf target/anipv-demo
    cargo run --features demo -- demo target/anipv-demo
    ANIPV_HOME=target/anipv-demo cargo run --features demo
