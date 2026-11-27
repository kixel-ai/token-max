# Local validation; the merge queue runs this plus the Windows tests, and macOS runs the tests after merges that touch the build.
# Cheapest first, so a dangling quest link fails before the build.
check:
    quest check
    cargo fmt --check
    cargo clippy --locked --all-targets -- -D warnings
    cargo test --locked

run *args:
    cargo run --release --locked -- {{args}}
