# Local validation; CI runs this plus the tests on macOS and Windows.
check:
    cargo fmt --check
    cargo clippy --locked --all-targets -- -D warnings
    cargo test --locked
    quest check

run *args:
    cargo run --release --locked -- {{args}}
