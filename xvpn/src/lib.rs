pub mod cli;
pub mod commands;
pub mod config;
pub mod network;
pub mod profiles;
pub mod supervisor;

/// Errors are plain strings: every failure is printed for a human, usually
/// alongside the stderr of whatever tool produced it.
pub type Error = String;

pub type Result<T> = std::result::Result<T, Error>;
