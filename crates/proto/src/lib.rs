//! What `srmd` and its clients share: the data model, the wire protocol, the config
//! file, and the plumbing for reaching the daemon.

pub mod client;
pub mod config;
pub mod model;
pub mod wire;

pub use model::*;
