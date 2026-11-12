//! The `token-max` command.

/// A full history parses millions of short-lived JSON values among millions of long-lived
/// records; glibc's allocator fragments under that mix and spends a quarter of the run in
/// `malloc`, while mimalloc keeps both the time and the peak lower.
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> std::process::ExitCode {
    token_max::cli::main()
}
