# Local validation; the merge queue runs this plus the Windows tests, and the macOS tests run after each merge.
# Cheapest first, so a dangling quest link fails before the build.
check:
    quest check
    cargo fmt --check
    cargo clippy --locked --all-targets -- -D warnings
    cargo test --locked

run *args:
    cargo run --release --locked -- {{args}}
