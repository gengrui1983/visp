//! visp-daemon library crate.
//!
//! Library target that makes the crate's public API available to
//! integration tests and potential downstream users.  Re-exports the
//! config module.
//!
//! The daemon binary (`main.rs`) is a separate target.

pub mod bus;
pub mod config;
pub mod hook_map;
pub mod hook_runtime;
pub mod reload;
pub mod watch;
