//! Catalog of MCP servers, projected project by project into Kiro's `mcp.json`.
//!
//! The core (`core`) stays strictly ignorant of the UI so both TUI views reuse
//! the same logic.

pub mod cli;
pub mod core;
pub mod ui;
