//! Terrarium's reusable execution library.
//!
//! The CLI is an adapter over these modules. Other frontends, including a future service for a
//! Web UI, should call the library instead of spawning the binary or parsing terminal output.

mod agent;
pub mod cli;
mod config;
mod fs;
mod kernel;
mod llm;
mod registry;
mod session;

pub use config::{Config, ProfileConfig, ProviderConfig, ResolvedProfile};
pub use fs::Mount;
pub use kernel::{ErrorKind, Kernel, Outcome, RunError, Termination, WriteSummary};

pub(crate) use kernel::{add_mount, contract_for, eval_js, MAX_TIMEOUT_MS, MEM_LIMIT};
