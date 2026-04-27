//! Binary entry point for the `kei` CLI.
//!
//! All real logic lives in the library at `src/lib.rs`. This shim exists so
//! the lib is the source of truth for everything: integration tests, fuzz
//! harnesses, and any future companion binary all consume the same module
//! tree without going through the binary.

fn main() -> std::process::ExitCode {
    kei::main_inner()
}
