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

# Build with the minimum supported Rust version (rustup toolchain 1.89).
msrv:
    cargo +1.89 check --all-targets --locked

# Apply clippy's suggestions, then format.
fix:
    cargo clippy --fix --allow-dirty --all-targets
    cargo fmt --all

# Install anipv to ~/.cargo/bin (release build).
install:
    cargo install --path . --locked

# Run anipv with arguments, e.g. `just run next`.
run *args:
    cargo run -- {{args}}

# Write the man page to ./anipv.1.
man:
    cargo run -q -- man > anipv.1

# Print shell completions, e.g. `just completions fish`.
completions shell:
    cargo run -q -- completions {{shell}}

# Regenerate README screenshots (SVG) from the demo library.
screenshots:
    cargo build
    cargo run --example screenshots

# Re-record the README GIF from assets/tapes/all.tape (needs vhs and ttyd).
record: _need-vhs demo-home
    PATH="{{justfile_directory()}}/target/debug:$PATH" vhs assets/tapes/all.tape

# Fresh demo library in assets/demo-home, as the recordings expect.
demo-home:
    rm -rf assets/demo-home
    cargo run -q -- demo assets/demo-home

_need-vhs:
    #!/bin/sh
    for tool in vhs ttyd; do
        command -v "$tool" >/dev/null || { echo "error: just record needs $tool on PATH (see https://github.com/charmbracelet/vhs)" >&2; exit 1; }
    done

# Update golden files after intentional changes (review the diff!).
update-fixtures:
    UPDATE_FIXTURES=1 cargo test --test parser_corpus --test docs
    cargo insta test --review

# Try the TUI against a throwaway demo library.
demo:
    rm -rf /tmp/anipv-demo
    cargo run -- demo /tmp/anipv-demo
    ANIPV_HOME=/tmp/anipv-demo cargo run
