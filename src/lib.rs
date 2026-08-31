//! Terrarium's reusable execution library.
//!
//! The CLI is an adapter over these modules. Other frontends, including a future service for a
//! Web UI, should call the library instead of spawning the binary or parsing terminal output.

mod agent;
mod auth;
pub mod cli;
mod config;
mod fs;
mod kernel;
mod llm;
mod registry;
mod session;

pub use auth::{Authorizer, Decision, ResolvedAccessRequest};
pub use config::{Config, ProfileConfig, ProviderConfig, ResolvedProfile};
pub use fs::{FilesystemMode, RunFilesystemAuthority, WriteScope};
pub use kernel::{ErrorKind, Kernel, Outcome, RunError, Termination, WriteSummary};

pub(crate) use kernel::{contract, eval_js, MAX_TIMEOUT_MS, MEM_LIMIT};
