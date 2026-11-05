# Everything CI runs.
check:
    cargo fmt --check
    cargo clippy --all-targets --locked -- -D warnings
    cargo test --locked
    quest check
