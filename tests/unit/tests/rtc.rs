//! Host unit tests for the pure RTC/CMOS decoder. The tests live next to the code
//! in `src/rtc_pure.rs` under `#[cfg(test)]`; this harness includes the module so
//! `cargo test` (host target) compiles and runs them.
#[path = "../../../src/rtc_pure.rs"]
mod rtc_pure;
