//! The Network tool: a virtualised requests table with tabs, filters and a waterfall.

pub mod body;
mod details;
pub mod model;
mod table;
mod toolbar;
pub mod view;

pub use view::{NetworkEvent, NetworkView};
