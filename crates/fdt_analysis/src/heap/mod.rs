//! Dart heap snapshots: a streaming parser into a compact struct-of-arrays graph, a full
//! decoder for tests and single-object reads, and an encoder (with a builder) for fixtures.

pub mod details;
pub mod diff;
pub mod dominators;
pub mod encode;
pub mod format;
pub mod leaks;
pub mod sdk;

pub use details::{ObjectDetails, labelled_path, object_details};
pub use diff::{default_baseline, diff};
pub use dominators::{
    Analysis, AnalyzeInput, Dominated, analyze, build_index, dominators, shortest_retainers,
    weak_classes,
};
pub use encode::{SnapshotBuilder, encode_full};
pub use format::{
    ClassDef, ExternalDef, FieldDef, FullSnapshot, Graph, HeapError, HeapHeader, Kind,
    OFFSET_STRIDE, ObjectRecord, Value, decode_full, parse, read_object,
};
pub use leaks::{classify, edge, path_text};
