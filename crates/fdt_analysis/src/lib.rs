//! Analysis over store snapshots: phase breakdown, rebuild attribution, correlation and the
//! user-code classifier. Pure data; no GPUI.

pub mod breakdown;
pub mod correlate;
pub mod cpu_tree;
pub mod explain;
pub mod framework_widgets;
pub mod heap;
pub mod rebuilds;
pub mod user_code;

#[cfg(test)]
pub(crate) mod testkit;
