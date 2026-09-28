//! Command-line surface. Argument parsing is done by hand: the surface is small
//! and fixed, and keeping dependencies out is what keeps the binary small.

pub mod args;
pub mod commands;
pub mod migration;

pub use args::{parse_args, Command, ParseError};
