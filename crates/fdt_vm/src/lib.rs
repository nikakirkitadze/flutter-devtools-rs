//! VM service access for `fdt`: URL parsing, a JSON-RPC 2.0 client over WebSocket, and a
//! transcript recorder. This crate has no UI dependencies.

pub mod rpc;
pub mod tap;
pub mod uri;

#[cfg(any(test, feature = "testing"))]
pub mod testing;
