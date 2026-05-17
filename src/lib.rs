//! `rit` as a library. Exposes the modules so integration tests and
//! benchmarks (which compile as separate crates) can drive the internals
//! without going through main.rs.
//!
//! `src/main.rs` is the binary entry point; everything it needs lives
//! here.

pub mod app;
pub mod git;
pub mod model;
pub mod ui;
