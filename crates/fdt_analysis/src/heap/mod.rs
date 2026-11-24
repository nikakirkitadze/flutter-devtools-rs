//! Dart heap snapshots: a streaming parser into a compact struct-of-arrays graph, a full
//! decoder for tests and single-object reads, and an encoder (with a builder) for fixtures.

pub mod encode;
pub mod format;

pub use encode::{SnapshotBuilder, encode_full};
pub use format::{
    ClassDef, ExternalDef, FieldDef, FullSnapshot, Graph, HeapError, HeapHeader, Kind,
    OFFSET_STRIDE, ObjectRecord, Value, decode_full, parse, read_object,
};
