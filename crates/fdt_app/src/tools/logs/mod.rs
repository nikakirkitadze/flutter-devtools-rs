//! The Logs tool: a virtualised list with follow mode, inline expansion and filters.

mod details;
pub mod model;
mod toolbar;
pub mod view;

pub use view::{LogsEvent, LogsView};
