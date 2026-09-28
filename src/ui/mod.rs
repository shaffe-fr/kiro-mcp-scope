//! Terminal UI. The state and its transitions live in view modules and are
//! unit-testable without a terminal; `app` runs the ratatui event loop.

pub mod app;
pub mod matrix_view;
pub mod project_view;
