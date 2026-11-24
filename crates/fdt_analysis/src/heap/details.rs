//! One object's fields and labelled retaining path, read from the snapshot file on demand.

use std::collections::HashMap;
use std::fs::File;
use std::io;

use fdt_store::heap::{Edge, HeapIndex, HeapSummary, PathHop};

use super::format::{ObjectRecord, Value, read_object};
use super::leaks::{holder_of, label};

#[derive(Debug, Clone)]
pub struct ObjectDetails {
    /// The object's own value: a string, a number, a length.
    pub value: Option<String>,
    /// `(edge, class, node, brief value)` of the referents, in field order. `node` is `None`
    /// for an unreachable referent.
    pub fields: Vec<(Edge, u32, Option<u32>, Option<String>)>,
    /// How many references the object has; `fields` is the first `max_fields` of them.
    pub fields_total: u32,
    pub path: Vec<PathHop>,
}

/// A short rendering of a value, or `None` for an object that has none.
fn brief(v: &Value) -> Option<String> {
    Some(match v {
        Value::None => return None,
        Value::Null => "null".to_owned(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Double(d) => d.to_string(),
        Value::Latin1 { len, bytes } => quoted(v.text()?, bytes.len() < *len as usize),
        Value::Utf16 { len, units } => quoted(v.text()?, units.len() < *len as usize),
        Value::Length(n) => format!("length {n}"),
        Value::Name(name) => name.clone(),
    })
}

fn quoted(text: String, cut: bool) -> String {
    format!("\"{text}{}\"", if cut { "…" } else { "" })
}

fn open(index: &HeapIndex) -> io::Result<File> {
    File::open(index.file().path())
}

fn record(index: &HeapIndex, file: &File, node: u32) -> io::Result<ObjectRecord> {
    read_object(file, index.offsets(), index.mint_class(), index.orig(node))
}

fn class_name(summary: &HeapSummary, class: u32) -> &str {
    summary
        .classes
        .get(class as usize)
        .map_or("?", |c| &c.name[..])
}

/// The label of the reference at ref position `pos` of `parent` (whose class is `class`),
/// which reaches `child`.
fn edge_at(
    index: &HeapIndex,
    summary: &HeapSummary,
    parent_class: u32,
    pos: usize,
    child: Option<u32>,
) -> Edge {
    let name = class_name(summary, parent_class);
    let field = summary.classes.get(parent_class as usize).and_then(|c| {
        c.fields
            .iter()
            .find(|(i, _)| *i as usize == pos)
            .map(|(_, f)| &f[..])
    });
    let root_label = child.map_or(String::new(), |c| {
        if index.class_of(c) == 4 {
            "isolate".to_owned()
        } else {
            index
                .synthetic_name(c)
                .unwrap_or_else(|| class_name(summary, index.class_of(c)))
                .to_owned()
        }
    });
    label(holder_of(parent_class, name), name, field, pos, &root_label)
}

/// The path from the root to `node`, with every hop's edge read from its retainer's record.
/// Task 8's samples use it.
pub fn labelled_path(
    index: &HeapIndex,
    summary: &HeapSummary,
    node: u32,
) -> io::Result<Vec<PathHop>> {
    let nodes = index.path(node);
    if nodes.is_empty() {
        return Ok(Vec::new());
    }
    let file = open(index)?;
    let mut out = Vec::with_capacity(nodes.len());
    for (i, &n) in nodes.iter().enumerate() {
        let edge = match nodes.get(i + 1) {
            None => Edge::None,
            Some(&next) => {
                let rec = record(index, &file, n)?;
                let target = index.orig(next);
                match rec.refs.iter().position(|&r| r == target) {
                    Some(pos) => edge_at(index, summary, index.class_of(n), pos, Some(next)),
                    None => Edge::Internal(0),
                }
            }
        };
        out.push(PathHop {
            node: n,
            class: index.class_of(n),
            retained: index.retained(n),
            edge,
        });
    }
    Ok(out)
}

/// An object's value, its first `max_fields` references with their referents' values, and its
/// labelled path.
pub fn object_details(
    index: &HeapIndex,
    summary: &HeapSummary,
    node: u32,
    max_fields: usize,
) -> io::Result<ObjectDetails> {
    if node as usize >= index.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no such object",
        ));
    }
    let file = open(index)?;
    let rec = record(index, &file, node)?;
    let class = index.class_of(node);

    let shown: Vec<(usize, u32)> = rec
        .refs
        .iter()
        .enumerate()
        .filter(|(_, r)| **r != 0)
        .map(|(pos, &r)| (pos, r))
        .collect();
    let fields_total = u32::try_from(shown.len()).unwrap_or(u32::MAX);
    let shown = &shown[..shown.len().min(max_fields)];

    // One pass finds the nodes of every shown referent.
    let mut node_of: HashMap<u32, u32> = shown.iter().map(|&(_, r)| (r, u32::MAX)).collect();
    if !node_of.is_empty() {
        for n in 0..index.len() as u32 {
            if let Some(slot) = node_of.get_mut(&index.orig(n)) {
                *slot = n;
            }
        }
    }
    let mut fields = Vec::with_capacity(shown.len());
    for &(pos, r) in shown {
        let target = node_of.get(&r).copied().filter(|&n| n != u32::MAX);
        let referent = read_object(&file, index.offsets(), index.mint_class(), r)?;
        fields.push((
            edge_at(index, summary, class, pos, target),
            referent.class,
            target,
            brief(&referent.value),
        ));
    }
    Ok(ObjectDetails {
        value: brief(&rec.value),
        fields,
        fields_total,
        path: labelled_path(index, summary, node)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::dominators::{AnalyzeInput, analyze};
    use crate::heap::encode::{SnapshotBuilder, encode_full};
    use fdt_store::heap::HeapFile;
    use fdt_store::run::Mode;
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn object_details_reads_fields_and_values() {
        let mut b = SnapshotBuilder::new();
        let string = b.class("_OneByteString", "dart:core", &[]);
        let list = b.class("_List", "dart:core", &["type_arguments", "length_"]);
        let int = b.class("_Smi", "dart:core", &[]);
        let long = "x".repeat(200);
        let name = b.object(
            string,
            24,
            Value::Latin1 {
                len: 200,
                bytes: long.as_bytes().to_vec(),
            },
            &[],
        );
        let short = b.object(
            string,
            24,
            Value::Latin1 {
                len: 5,
                bytes: b"hello".to_vec(),
            },
            &[],
        );
        let items = b.object(list, 40, Value::Length(3), &[0, 0, short, short, short]);
        let count = b.object(int, 0, Value::Int(42), &[]);
        let holder = b.class("Holder", "package:app/a.dart", &["name", "items", "count"]);
        // A fourth ref has no field name.
        let h = b.object(holder, 48, Value::None, &[name, items, count, short]);
        b.static_field("held", h);

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.dartheap");
        std::fs::write(&path, encode_full(&b.finish())).unwrap();
        let input = AnalyzeInput {
            file: Arc::new(HeapFile::new(path)),
            flutter_version: None,
            mode: Mode::Profile,
            root_package: None,
        };
        let a = analyze(&input, &AtomicBool::new(false), &mut |_| {}).unwrap();
        let node = (0..a.index.len() as u32)
            .find(|&n| a.index.orig(n) == h)
            .unwrap();
        let d = object_details(&a.index, &a.summary, node, 3).unwrap();
        assert_eq!(d.value, None);
        assert_eq!(d.fields_total, 4);
        assert_eq!(d.fields.len(), 3, "capped at max_fields");
        let edges: Vec<&Edge> = d.fields.iter().map(|f| &f.0).collect();
        assert_eq!(
            edges,
            [
                &Edge::Field("name".into()),
                &Edge::Field("items".into()),
                &Edge::Field("count".into())
            ]
        );
        let name_brief = d.fields[0].3.as_deref().unwrap();
        assert!(
            name_brief.starts_with("\"xxxx") && name_brief.ends_with("…\""),
            "{name_brief}"
        );
        assert_eq!(d.fields[1].3.as_deref(), Some("length 3"));
        assert_eq!(d.fields[2].3.as_deref(), Some("42"));
        assert!(d.fields.iter().all(|f| f.2.is_some()), "all reachable");
        assert_eq!(
            a.summary.classes[d.fields[1].1 as usize].name.as_ref(),
            "_List"
        );

        // The fourth ref is unnamed: internal, and a short string is shown whole.
        let all = object_details(&a.index, &a.summary, node, 10).unwrap();
        assert_eq!(all.fields[3].0, Edge::Internal(3));
        assert_eq!(all.fields[3].3.as_deref(), Some("\"hello\""));

        // A string's own value; a list's length.
        let short_node = all.fields[3].2.unwrap();
        let own = object_details(&a.index, &a.summary, short_node, 10).unwrap();
        assert_eq!(own.value.as_deref(), Some("\"hello\""));
        assert!(own.fields.is_empty());

        // The path is labelled from the retainers' records, root first.
        assert_eq!(d.path.first().unwrap().node, 0);
        assert_eq!(d.path[0].edge, Edge::RootSlice("isolate".into()));
        assert_eq!(d.path[1].edge, Edge::Static("held".into()));
        assert_eq!(d.path.last().unwrap().node, node);
        let direct = labelled_path(&a.index, &a.summary, node).unwrap();
        assert_eq!(direct.len(), d.path.len());

        assert!(object_details(&a.index, &a.summary, 9_999, 1).is_err());
        assert!(
            labelled_path(&a.index, &a.summary, 9_999)
                .unwrap()
                .is_empty()
        );
    }
}
