//! vatio — Spain's electricity prices and grid status.
//!
//! The crate is a command-line tool first. This library is what the CLI and the
//! built-in MCP server share; [`Vatio`] is the entry point and [`model`] holds the
//! output documents.
//!
//! The JSON output, exit codes and MCP tools are the stable interface. The Rust API
//! here is not: it may change in any release.

pub mod cache;
pub mod error;
pub mod grid;
pub mod mcp;
pub mod model;
pub mod redata;
pub mod render;
pub mod service;
pub mod text;
pub mod time;
pub mod window;

pub use error::{Error, Result};
pub use service::{CheapestQuery, Options, Vatio};
