//! Loomabase: a transactional offline-first synchronization core.

#![forbid(unsafe_code)]

#[cfg(feature = "server")]
pub mod auth;
pub mod client;
pub mod codegen;
pub mod crdt;
pub mod error;
pub mod explain;
#[cfg(feature = "server")]
pub mod http;
pub mod policy;
pub mod replica;
pub mod schema;
pub mod server;
pub mod simulator;

pub use error::{Result, SyncError};
