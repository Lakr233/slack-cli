//! Read your own Slack data from the Slack desktop app's local cache.
//!
//! Slack desktop persists a snapshot of its Redux store per workspace in
//! IndexedDB. This crate finds those snapshots, decodes them (snappy, Blink
//! and V8 structured clone) and turns them into channels, members, messages
//! and files. It never touches the network and never writes to Slack's data.

mod cli;
pub mod decode;
pub mod redact;
pub mod store;

pub use cli::{run, Term};
