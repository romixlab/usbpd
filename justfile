# Everything CI builds: host checks and the firmware examples (needs the embassy submodule)
build:
    .github/ci/build.sh

# Workspace tests; `just test --changed` tests only what changed against main
test *flags:
    if [ "{{flags}}" = "--changed" ]; then if command -v changedtests >/dev/null 2>&1; then f=$(changedtests) && if [ -n "$f" ]; then cargo nextest run --workspace -E "$f"; fi; else echo "changedtests not found; running the full test suite instead" >&2; {{just_executable()}} test-all; fi; else {{just_executable()}} test-all {{flags}}; fi

# All workspace tests: nextest, then the doctests
test-all:
    cargo nextest run --workspace
    cargo test --workspace --doc

# Nightly rustfmt check (rustfmt.toml uses nightly options) and clippy without warnings
lint:
    cargo +nightly fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
