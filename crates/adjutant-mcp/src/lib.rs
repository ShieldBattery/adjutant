//! A deliberately small, read-only Model Context Protocol server for diagnostics.
//!
//! The database role remains the primary permission boundary. This crate adds
//! defence in depth: it accepts only a single query statement, starts every
//! request in a read-only transaction, and bounds the amount of data returned.

pub mod config;
pub mod database;
pub mod server;

pub use config::Config;
pub use server::McpServer;
