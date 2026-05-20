//! Library face of deckfile.
//!
//! The binary in `main.rs` is the main consumer, but exposing the
//! modules through `lib.rs` is what lets integration tests under
//! `tests/` (and external crates, in the future) reach internals
//! like the Slint screen pipeline without going through the CLI.

pub mod config;
pub mod daemon;
pub mod render;
pub mod slint_screen;
