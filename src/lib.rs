//! Terrarium's reusable execution library.
//!
//! The CLI is an adapter over these modules. Other frontends, including a future service for a
//! Web UI, should call the library instead of spawning the binary or parsing terminal output.

mod agent;
pub mod cli;
mod fs;
mod kernel;
mod llm;
mod registry;

pub use fs::Mount;
pub use kernel::{ErrorKind, Kernel, Outcome, RunError, Termination};

pub(crate) use kernel::{add_mount, contract, eval_js, truncate_utf8, MAX_TIMEOUT_MS, MEM_LIMIT};
