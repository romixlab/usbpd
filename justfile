# Everything CI builds: host checks and the firmware examples (needs the embassy submodule)
build:
    .github/ci/build.sh

# Workspace tests; `just test --changed` tests only what changed against main
test *flags:
    if [ "{{flags}}" = "--changed" ]; then tpm test changed -- cargo nextest run --workspace; else {{just_executable()}} test-all {{flags}}; fi

# All workspace tests: nextest, then the doctests
test-all:
    cargo nextest run --workspace
    cargo test --workspace --doc

# Nightly rustfmt check (rustfmt.toml uses nightly options) and clippy without warnings
lint:
    cargo +nightly fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
