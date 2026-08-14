//! Host unit tests for the pure cpio newc parser. The tests live next to the code
//! in `src/cpio.rs` under `#[cfg(test)]`; this harness just includes the module so
//! `cargo test` (host target) compiles and runs them.
#[path = "../../../src/cpio.rs"]
mod cpio;
