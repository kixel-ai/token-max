# Local validation; CI runs this plus the tests on macOS and Windows.
# Cheapest first, so a dangling quest link fails before the build.
check:
    quest check
    cargo fmt --check
    cargo clippy --locked --all-targets -- -D warnings
    cargo test --locked

run *args:
    cargo run --release --locked -- {{args}}
