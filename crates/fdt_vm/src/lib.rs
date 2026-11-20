//! VM service access for `fdt`: URL parsing, a JSON-RPC 2.0 client over WebSocket, a transcript
//! recorder, and the typed subset of the VM service and `dart:io` profiling protocols.
//! This crate has no UI dependencies.

pub mod dart_io;
pub mod discovery;
pub mod dtd;
pub mod events;
pub mod flutter;
pub mod perfetto;
pub mod rpc;
pub mod service;
pub mod tap;
pub mod timeline;
pub mod types;
pub mod uri;

#[cfg(any(test, feature = "testing"))]
pub mod testing;
