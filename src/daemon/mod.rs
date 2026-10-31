//! Persistent v2 index owner.
pub mod builder;
pub mod ipc;
pub mod query;
pub mod server;
pub mod view;

mod changes;
mod executable;
mod progress;
mod recovery;
mod watch;
mod writer;
