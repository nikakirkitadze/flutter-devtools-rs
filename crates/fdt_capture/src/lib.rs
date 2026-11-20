//! Live capture for `fdt`: a tokio runtime, the `Capture` handle, the connect sequence and the
//! ingest of a running app's logs, errors, navigation and frames into an `fdt_store` session.
//! This crate has no UI dependencies.

#[doc(hidden)]
pub mod bench_support;
mod capture;
mod config;
mod connect;
mod ingest;
mod lifecycle;
mod runtime;
mod status;

pub use capture::{Capture, Target};
pub use config::CaptureConfig;
pub use runtime::CaptureRuntime;
pub use status::ConnStatus;
