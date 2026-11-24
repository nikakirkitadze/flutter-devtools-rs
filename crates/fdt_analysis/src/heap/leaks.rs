//! The level-1 leak scan: lifecycle objects that are dead but still reachable, each one's
//! readable retaining path and likely culprit, what it keeps alive, and what the HTTP
//! recording holds.
//!
//! The scan runs between the breadth-first retainers and the depth-first numbering, while the
//! forward references still exist. Everything that needs node ids or retained sizes is filled
//! in afterwards by [`finalize`], once the held index has renumbered the objects.

use std::collections::HashSet;
use std::fs::File;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use fdt_store::heap::{
    Check, Edge, HeapIndex, HeapSummary, HttpHeld, LeakChecks, LeakFinding, LeakKind,
    LifecycleKind, PathHop, Tracked,
};

use super::details::labelled_path;
use super::format::{ClassDef, Graph, HeapError, Kind, read_object};
use super::sdk::SdkFields;
use crate::user_code::is_user_library;

/// Findings that keep a path; the rest are only counted.
pub const MAX_FINDINGS: usize = 200;

/// How many dominator children a finding lists as what it keeps alive.
pub const KEEPS: usize = 5;

/// How often the long loops look at the cancel flag.
const CANCEL_MASK: usize = 0xFFFF;

// ---- classification ----

/// True when the class's fields start with all of `base`'s, in order and at the same positions:
/// the snapshot has no hierarchy, so this is how a subclass is recognised.
fn extends(base: &ClassDef, c: &ClassDef) -> bool {
    !base.fields.is_empty()
        && c.fields.len() >= base.fields.len()
        && base
            .fields
            .iter()
            .zip(&c.fields)
            .all(|(b, f)| b.name == f.name && b.index == f.index)
}

/// The lifecycle kind of every class, by snapshot class id, by the field-name prefix rule. The
/// base classes count too. A class that matches several takes `State`, then `Element`, then
/// `Notifier`.
pub fn classify(g: &Graph, row: &SdkFields) -> Vec<Option<LifecycleKind>> {
    let bases = [
        (LifecycleKind::State, row.state.0, row.framework_uri),
        (LifecycleKind::Element, row.element.0, row.framework_uri),
        (LifecycleKind::Notifier, row.notifier.0, row.notifier_uri),
    ]
    .map(|(kind, name, uri)| {
        (
            kind,
            g.find_class(name, uri).map(|c| &g.classes[c as usize]),
        )
    });
    g.classes
        .iter()
        .enumerate()
        .map(|(i, c)| {
            if i == 0 {
                return None;
            }
            bases
                .iter()
                .find(|(_, base)| base.is_some_and(|b| extends(b, c)))
                .map(|(kind, _)| *kind)
        })
        .collect()
}

/// `package:<root>/…`, or a `file:` URI that [`is_user_library`] accepts.
pub fn is_user_class(library_uri: &str, root_package: Option<&str>) -> bool {
    let in_root = root_package.is_some_and(|root| {
        library_uri
            .strip_prefix("package:")
            .and_then(|rest| rest.strip_prefix(root))
            .is_some_and(|rest| rest.starts_with('/'))
    });
    in_root || (library_uri.starts_with("file:") && is_user_library(Some(library_uri)))
}

// ---- edge labels ----

/// What holds the reference.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Holder {
    Root,
    Isolate,
    Class,
}

/// The holder of a reference, from its class id and name (the fixed layout puts `Root` at 1
/// and `Isolate` at 4).
pub(crate) fn holder_of(class: u32, name: &str) -> Holder {
    match (class, name) {
        (1, "Root") => Holder::Root,
        (4, "Isolate") => Holder::Isolate,
        _ => Holder::Class,
    }
}

/// The label of the reference at ref position `pos` of an object. `field` is the field name
/// at that position; `root_label` is what a root slice is called (only used for the root).
pub(crate) fn label(
    holder: Holder,
    class_name: &str,
    field: Option<&str>,
    pos: usize,
    root_label: &str,
) -> Edge {
    match holder {
        Holder::Root => return Edge::RootSlice(root_label.into()),
        Holder::Isolate => {
            return field.map_or(Edge::Internal(pos as u32), |f| Edge::Static(f.into()));
        }
        Holder::Class => {}
    }
    match class_name {
        "_List" | "_ImmutableList" if pos >= 2 => Edge::Element((pos - 2) as u32),
        "_Closure" if pos == 3 => Edge::ClosureContext,
        "Context" if pos >= 1 => Edge::Captured((pos - 1) as u32),
        _ => field.map_or(Edge::Internal(pos as u32), |f| Edge::Field(f.into())),
    }
}

/// The label of object `from`'s reference at ref position `pos`:
///
/// - from the root, `RootSlice` of the target's name data (`root_name`, which only the file
///   holds), or `"isolate"` for the isolate object;
/// - from the isolate object, `Static(field)`;
/// - a `_List` element, a closure's context, a context's captured variable;
/// - otherwise `Field(name)`, else `Internal(pos)`.
pub fn edge(g: &Graph, from: u32, pos: usize, root_name: Option<&str>) -> Edge {
    let cid = g.class_of.get(from as usize).copied().unwrap_or(0);
    let Some(class) = g.classes.get(cid as usize).filter(|_| cid != 0) else {
        return Edge::Internal(pos as u32);
    };
    let holder = holder_of(cid, &class.name);
    let mut root_label = String::new();
    if holder == Holder::Root {
        let target = g.refs_of(from).get(pos).copied().unwrap_or(0);
        let tc = g.class_of.get(target as usize).copied().unwrap_or(0);
        root_label = if tc == 4 {
            "isolate".into()
        } else {
            root_name.map_or_else(
                || {
                    g.classes
                        .get(tc as usize)
                        .map_or(String::new(), |c| c.name.clone())
                },
                str::to_owned,
            )
        };
    }
    label(
        holder,
        &class.name,
        g.field_name_at(cid, pos),
        pos,
        &root_label,
    )
}

// ---- the scan ----

/// A leaked object; paths are built in [`finalize`], for the largest only.
#[derive(Debug)]
pub struct RawFinding {
    pub kind: LeakKind,
    pub object: u32,
}

#[derive(Debug)]
pub struct Scan {
    /// By class id.
    pub user: Vec<bool>,
    /// By class id.
    pub lifecycle: Vec<Option<LifecycleKind>>,
    pub checks: LeakChecks,
    /// Every leaked object (8 B each).
    pub findings: Vec<RawFinding>,
    /// `(class, identity hash, object)` of reachable user and lifecycle objects with a hash.
    pub tracked: Vec<(u32, u32, u32)>,
    pub http_objects: Vec<u32>,
    pub http_lists: Vec<u32>,
}

fn cancelled(cancel: &AtomicBool) -> Result<(), HeapError> {
    if cancel.load(Ordering::Relaxed) {
        Err(HeapError::Cancelled)
    } else {
        Ok(())
    }
}

/// The text of a string object, read from the file. An I/O or decode failure is an error: it
/// must never read as "not defunct".
fn read_text(g: &Graph, file: &File, object: u32) -> Result<Option<String>, HeapError> {
    let rec = read_object(file, &g.offsets, g.mint_class, object)
        .map_err(|e| HeapError::Io(e.kind().to_string()))?;
    Ok(rec.value.text())
}

/// The first row whose base class for `class` exists in the snapshot.
fn first_row<'a>(
    g: &Graph,
    rows: &[&'a SdkFields],
    class: impl Fn(&SdkFields) -> (&'static str, &'static str),
) -> Option<(&'a SdkFields, u32)> {
    rows.iter().find_map(|&r| {
        let (name, uri) = class(r);
        g.find_class(name, uri).map(|c| (r, c))
    })
}

/// Runs the level-1 scan over reachable objects (`retainers[o] != 0 || o == 1`).
///
/// `file` is read for the strings the graph does not keep (the lifecycle enum's names); a read
/// failure fails the scan.
pub fn scan(
    g: &Graph,
    retainers: &[u32],
    file: &File,
    rows: &[&SdkFields],
    root_package: Option<&str>,
    cancel: &AtomicBool,
) -> Result<Scan, HeapError> {
    let n = g.object_count();
    let state = first_row(g, rows, |r| (r.state.0, r.framework_uri));
    let element = first_row(g, rows, |r| (r.element.0, r.framework_uri));
    let notifier = first_row(g, rows, |r| (r.notifier.0, r.notifier_uri));

    // Each kind is classified with the row that found its base class.
    let mut lifecycle: Vec<Option<LifecycleKind>> = vec![None; g.classes.len()];
    for (found, kinds) in [
        (state, &[LifecycleKind::State][..]),
        (element, &[LifecycleKind::Element][..]),
        (notifier, &[LifecycleKind::Notifier][..]),
    ] {
        if let Some((row, _)) = found {
            for (slot, k) in lifecycle.iter_mut().zip(classify(g, row)) {
                if slot.is_none() && k.is_some_and(|k| kinds.contains(&k)) {
                    *slot = k;
                }
            }
        }
    }
    let user: Vec<bool> = g
        .classes
        .iter()
        .enumerate()
        .map(|(i, c)| i != 0 && is_user_class(&c.library_uri, root_package))
        .collect();

    // What each check needs, and whether it can run.
    let state_field = state.map(|(r, c)| (r.state.1, c));
    let (widget_name, life_name) = element.map_or(("", ""), |(r, _)| (r.element.2, r.element.1));
    let disposed_name = notifier.map_or("", |(r, _)| r.notifier.2);
    let decide = |found: Option<u32>, fields: &[&str]| -> Check {
        let Some(base) = found else {
            return Check::NotFound;
        };
        match fields.iter().find(|f| g.field_index(base, f).is_none()) {
            Some(missing) => Check::Undecidable(Arc::from(*missing)),
            None => Check::Done,
        }
    };
    let states = decide(state.map(|s| s.1), &[state_field.map_or("", |s| s.0)]);
    // Either signal is decisive on its own (`unmount()` sets both): the lifecycle enum's name,
    // or a null `_widget`. The check is undecidable only when neither can run.
    let enum_cid = element.and_then(|(r, _)| g.find_class(r.element.3, r.framework_uri));
    let widget_ok = element.is_some_and(|(_, c)| g.field_index(c, widget_name).is_some());
    let name_pos_in_enum = element
        .zip(enum_cid)
        .and_then(|((r, _), e)| g.field_index(e, r.element.4));
    let life_ok = element.is_some_and(|(_, c)| g.field_index(c, life_name).is_some())
        && name_pos_in_enum.is_some();
    let elements = match element {
        None => Check::NotFound,
        Some(_) if widget_ok || life_ok => Check::Done,
        Some((r, c)) => {
            let mut missing = vec![widget_name];
            if g.field_index(c, life_name).is_none() {
                missing.push(life_name);
            } else if enum_cid.is_none() {
                missing.push(r.element.3);
            } else {
                missing.push(r.element.4);
            }
            Check::Undecidable(missing.join(", ").into())
        }
    };
    // Without the debug-only flag a disposed notifier looks like one never listened to.
    let notifiers = match decide(notifier.map(|e| e.1), &[disposed_name]) {
        Check::Undecidable(_) => Check::Undecidable(Arc::from("profile")),
        c => c,
    };

    // Which objects are the lifecycle enum's `defunct`.
    let mut defunct: HashSet<u32> = HashSet::new();
    if life_ok
        && let Some((row, _)) = element
        && let Some(enum_cid) = enum_cid
        && let Some(name_pos) = name_pos_in_enum
    {
        for o in 1..=n as u32 {
            if o as usize & CANCEL_MASK == 0 {
                cancelled(cancel)?;
            }
            if g.class_of[o as usize] == enum_cid
                && let Some(&s) = g.refs_of(o).get(name_pos)
                && s != 0
                && read_text(g, file, s)?.as_deref() == Some(row.element.5)
            {
                defunct.insert(o);
            }
        }
    }

    // Per-class ref positions, by class id: the first field of a name, so a subclass
    // re-declaring a private name does not shift them.
    let g_pos = |g: &Graph, c: usize, name: &str| g.field_index(c as u32, name);
    let state_pos: Vec<Option<usize>> = (0..g.classes.len())
        .map(|c| match (lifecycle[c], state_field) {
            (Some(LifecycleKind::State), Some((name, _))) if states == Check::Done => {
                g_pos(g, c, name)
            }
            _ => None,
        })
        .collect();
    let element_pos: Vec<Option<(Option<usize>, Option<usize>)>> = (0..g.classes.len())
        .map(|c| match lifecycle[c] {
            Some(LifecycleKind::Element) if elements == Check::Done => Some((
                g_pos(g, c, life_name).filter(|_| life_ok),
                g_pos(g, c, widget_name).filter(|_| widget_ok),
            )),
            _ => None,
        })
        .collect();
    let notifier_pos: Vec<Option<usize>> = (0..g.classes.len())
        .map(|c| match lifecycle[c] {
            Some(LifecycleKind::Notifier) if notifiers == Check::Done => g_pos(g, c, disposed_name),
            _ => None,
        })
        .collect();

    let http_cid = rows.iter().find_map(|r| g.find_class(r.http.0, r.http.1));

    let mut findings: Vec<RawFinding> = Vec::new();
    let mut tracked = Vec::new();
    let mut http_objects = Vec::new();
    let is_null = |r: Option<&u32>| r.is_some_and(|&r| r != 0 && g.kind[r as usize] == Kind::Null);

    for o in 2..=n as u32 {
        if o as usize & CANCEL_MASK == 0 {
            cancelled(cancel)?;
        }
        if retainers[o as usize] == 0 {
            continue;
        }
        let cid = g.class_of[o as usize] as usize;
        let refs = g.refs_of(o);
        let kind = if let Some(pos) = state_pos[cid] {
            is_null(refs.get(pos)).then_some(LeakKind::UnmountedState)
        } else if let Some((life, widget)) = element_pos[cid] {
            let by_name = life
                .and_then(|p| refs.get(p))
                .is_some_and(|r| defunct.contains(r));
            let no_widget = widget.is_some_and(|p| is_null(refs.get(p)));
            (by_name || no_widget).then_some(LeakKind::DefunctElement)
        } else if let Some(pos) = notifier_pos[cid] {
            refs.get(pos)
                .is_some_and(|&r| r != 0 && g.kind[r as usize] == Kind::True)
                .then_some(LeakKind::DisposedNotifier)
        } else {
            None
        };
        if let Some(kind) = kind {
            findings.push(RawFinding { kind, object: o });
        }
        let hash = g.identity_hash.get(o as usize).copied().unwrap_or(0);
        if hash != 0 && (user[cid] || lifecycle[cid].is_some()) {
            tracked.push((cid as u32, hash, o));
        }
        if Some(cid as u32) == http_cid {
            http_objects.push(o);
        }
    }
    cancelled(cancel)?;

    // The HTTP recording's other store, which `package:http_profile` clients fill.
    let mut http_lists = Vec::new();
    if let Some(r) = rows.first() {
        for v in g.static_values(r.http.2) {
            let reachable = v == 1 || retainers.get(v as usize).is_some_and(|&p| p != 0);
            let class = &g.classes[g.class_of[v as usize] as usize];
            if reachable && class.name == "_GrowableList" {
                http_lists.push(v);
            }
        }
    }

    Ok(Scan {
        user,
        lifecycle,
        checks: LeakChecks {
            states,
            elements,
            notifiers,
        },
        findings,
        tracked,
        http_objects,
        http_lists,
    })
}

// ---- after renumbering ----

pub struct Finalized {
    pub findings: Vec<LeakFinding>,
    pub total: u32,
    pub tracked: Vec<Tracked>,
    pub http: Option<HttpHeld>,
}

/// The culprit of a path: the last hop before the leaked object whose class is a user class,
/// else the first `Static` hop.
fn culprit_of(summary: &HeapSummary, path: &[PathHop]) -> Option<u16> {
    let before = path.len().checked_sub(1)?;
    path[..before]
        .iter()
        .rposition(|h| {
            summary
                .classes
                .get(h.class as usize)
                .is_some_and(|c| c.user)
        })
        .or_else(|| {
            path[..before]
                .iter()
                .position(|h| matches!(h.edge, Edge::Static(_)))
        })
        .and_then(|i| u16::try_from(i).ok())
}

/// Fills in node ids, retained sizes and what each finding keeps alive. `node_of` maps an
/// object to its node, `u32::MAX` if unreachable. Only the [`MAX_FINDINGS`] largest findings
/// get a path, labelled from the snapshot file; `summary` supplies the class names and fields
/// (its own findings are ignored).
pub fn finalize(
    scan: Scan,
    index: &HeapIndex,
    node_of: &[u32],
    summary: &HeapSummary,
) -> Result<Finalized, HeapError> {
    let node = |o: u32| node_of.get(o as usize).copied().filter(|&n| n != u32::MAX);
    let mut leaked: Vec<(LeakKind, u32)> = scan
        .findings
        .iter()
        .filter_map(|f| Some((f.kind, node(f.object)?)))
        .collect();
    let total = u32::try_from(leaked.len()).unwrap_or(u32::MAX);
    leaked.sort_by_key(|&(_, n)| (std::cmp::Reverse(index.retained(n)), n));
    leaked.truncate(MAX_FINDINGS);
    let findings = leaked
        .into_iter()
        .map(|(kind, n)| {
            let path = labelled_path(index, summary, n)
                .map_err(|e| HeapError::Io(e.kind().to_string()))?;
            let keeps: Vec<(u32, u64)> = index
                .children(n)
                .take(KEEPS)
                .map(|c| (index.class_of(c), index.retained(c)))
                .collect();
            Ok(LeakFinding {
                kind,
                node: n,
                class: index.class_of(n),
                retained: index.retained(n),
                culprit: culprit_of(summary, &path),
                path: path.into(),
                keeps: keeps.into(),
            })
        })
        .collect::<Result<Vec<_>, HeapError>>()?;

    let mut raw = scan.tracked;
    raw.sort_unstable();
    let mut tracked: Vec<Tracked> = Vec::new();
    let mut at = 0;
    while at < raw.len() {
        let class = raw[at].0;
        let end = raw[at..]
            .iter()
            .position(|t| t.0 != class)
            .map_or(raw.len(), |p| at + p);
        let (mut hashes, mut nodes) = (Vec::new(), Vec::new());
        for &(_, hash, o) in &raw[at..end] {
            if let Some(n) = node(o) {
                hashes.push(hash);
                nodes.push(n);
            }
        }
        if !hashes.is_empty() {
            tracked.push(Tracked {
                class,
                hashes: hashes.into(),
                nodes: nodes.into(),
            });
        }
        at = end;
    }

    let held = |objects: &[u32]| -> u64 {
        objects
            .iter()
            .filter_map(|&o| node(o))
            .map(|n| index.retained(n))
            .sum()
    };
    let http = (!scan.http_objects.is_empty() || !scan.http_lists.is_empty()).then(|| HttpHeld {
        bytes: held(&scan.http_objects) + held(&scan.http_lists),
        requests: scan.http_objects.len() as u64,
    });
    Ok(Finalized {
        findings,
        total,
        tracked,
        http,
    })
}

// ---- path text ----

fn class_name(summary: &HeapSummary, class: u32) -> &str {
    summary
        .classes
        .get(class as usize)
        .map_or("?", |c| &c.name[..])
}

/// A path as one line, for example `Root → static instance → AppEvents._listeners → _List[0] →
/// _Closure (closure context) → Context (captured 0) → _LeakyScreenState`. A path that does not
/// start at the root (the index keeps only the 200 hops nearest the object) starts with `… → `.
pub fn path_text(summary: &HeapSummary, path: &[PathHop]) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(path.len() + 1);
    if path.first().is_some_and(|h| h.node != 0) {
        parts.push("…".into());
    }
    for hop in path {
        let class = class_name(summary, hop.class);
        parts.push(match &hop.edge {
            Edge::None => class.to_owned(),
            Edge::Static(name) => format!("static {name}"),
            Edge::RootSlice(name) if &**name == "isolate" => "Root".to_owned(),
            Edge::RootSlice(name) => format!("Root ({name})"),
            Edge::Field(name) => format!("{class}.{name}"),
            Edge::Element(i) => format!("{class}[{i}]"),
            Edge::ClosureContext => format!("{class} (closure context)"),
            Edge::Captured(i) => format!("{class} (captured {i})"),
            Edge::Internal(pos) => format!("{class} (ref {pos})"),
        });
    }
    parts.join(" → ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::dominators::{Analysis, AnalyzeInput, analyze};
    use crate::heap::encode::{SnapshotBuilder, encode_full};
    use crate::heap::format::{Value, parse};
    use crate::heap::sdk;
    use fdt_store::heap::HeapFile;
    use fdt_store::heap::MAX_PATH_HOPS;
    use fdt_store::run::Mode;

    const FW: &str = "package:flutter/src/widgets/framework.dart";
    const NOTIFIER: &str = "package:flutter/src/foundation/change_notifier.dart";
    const LISTEN: &str = "package:listen/src/listen.dart";
    const APP: &str = "package:jank_lab/scenarios/leaks.dart";

    /// A builder with the shared framework classes, in profile or debug layout (H15).
    struct Lab {
        b: SnapshotBuilder,
        debug: bool,
        null: u32,
        yes: u32,
        no: u32,
        element: u32,
        lifecycle: u32,
        notifier: u32,
        list: u32,
        string: u32,
    }

    impl Lab {
        fn new(debug: bool, notifier_uri: &str) -> Lab {
            let mut b = SnapshotBuilder::new();
            let nul = b.class("Null", "dart:core", &[]);
            let boolean = b.class("bool", "dart:core", &[]);
            let string = b.class("_OneByteString", "dart:core", &[]);
            let list = b.class("_List", "dart:core", &["type_arguments", "length_"]);
            let mut s = vec!["type_arguments", "_widget", "_element"];
            let mut n = vec!["_count", "_listeners", "_notificationCallStackDepth"];
            if debug {
                s.push("_debugLifecycleState");
                n.push("_debugDisposed");
            }
            b.class("State", FW, &s);
            let element = b.class("Element", FW, &["_parent", "_lifecycleState", "_widget"]);
            let lifecycle = b.class("_ElementLifecycle", FW, &["index", "_name"]);
            let notifier = b.class("ChangeNotifier", notifier_uri, &n);
            let null = b.object(nul, 0, Value::Null, &[]);
            let yes = b.object(boolean, 0, Value::Bool(true), &[]);
            let no = b.object(boolean, 0, Value::Bool(false), &[]);
            Lab {
                b,
                debug,
                null,
                yes,
                no,
                element,
                lifecycle,
                notifier,
                list,
                string,
            }
        }

        fn text(&mut self, s: &str) -> u32 {
            let class = self.string;
            self.b.object(
                class,
                16,
                Value::Latin1 {
                    len: s.len() as u32,
                    bytes: s.as_bytes().to_vec(),
                },
                &[],
            )
        }

        /// A `_List` of `items`, held by a static called `name`.
        fn list_static(&mut self, name: &str, items: &[u32]) -> u32 {
            let mut refs = vec![0, 0];
            refs.extend(items);
            let l = self.b.object(
                self.list,
                16 + 8 * items.len() as u64,
                Value::Length(items.len() as u64),
                &refs,
            );
            self.b.static_field(name, l);
            l
        }

        /// An object of a subclass of `State` (profile or debug layout) with `element`.
        fn state_of(&mut self, class: u32, element: u32) -> u32 {
            let mut refs = vec![0, 0, element];
            if self.debug {
                refs.push(0);
            }
            self.b.object(class, 32, Value::None, &refs)
        }

        fn user_state_class(&mut self, name: &str) -> u32 {
            let mut f = vec!["type_arguments", "_widget", "_element"];
            if self.debug {
                f.push("_debugLifecycleState");
            }
            f.push("_text");
            self.b.class(name, APP, &f)
        }

        fn run(self, version: Option<&str>, root: Option<&str>) -> (tempfile::TempDir, Analysis) {
            run_snapshot(self.b, version, root)
        }
    }

    fn run_snapshot(
        b: SnapshotBuilder,
        version: Option<&str>,
        root: Option<&str>,
    ) -> (tempfile::TempDir, Analysis) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.dartheap");
        std::fs::write(&path, encode_full(&b.finish())).unwrap();
        let input = AnalyzeInput {
            file: Arc::new(HeapFile::new(path)),
            flutter_version: version.map(str::to_owned),
            mode: Mode::Profile,
            root_package: root.map(str::to_owned),
        };
        let a = analyze(&input, &AtomicBool::new(false), &mut |_| {}).expect("analyze");
        (dir, a)
    }

    fn names(a: &Analysis, f: &LeakFinding) -> Vec<String> {
        f.path
            .iter()
            .map(|h| a.summary.classes[h.class as usize].name.to_string())
            .collect()
    }

    /// `AppEvents.instance -> _listeners -> [closure -> context -> leaked state -> _text]`.
    fn jank_lab_leak(lab: &mut Lab) -> u32 {
        let state_class = lab.user_state_class("_LeakyScreenState");
        let controller = lab.b.class(
            "TextEditingController",
            "package:flutter/src/widgets/editable_text.dart",
            &["_listeners"],
        );
        let tec = lab.b.object(controller, 48, Value::None, &[0]);
        let leaked = {
            let mut refs = vec![0, 0, lab.null];
            if lab.debug {
                refs.push(0);
            }
            refs.push(tec);
            lab.b.object(state_class, 32, Value::None, &refs)
        };
        let context_class = lab.b.class("Context", "dart:core", &["parent_"]);
        let ctx = lab.b.object(context_class, 24, Value::None, &[0, leaked]);
        let closure_class = lab.b.class(
            "_Closure",
            "dart:core",
            &[
                "instantiator_type_arguments",
                "function_type_arguments",
                "function",
            ],
        );
        let closure = lab
            .b
            .object(closure_class, 32, Value::None, &[0, 0, 0, ctx]);
        let list = lab
            .b
            .object(lab.list, 32, Value::Length(1), &[0, 0, closure]);
        let events_class = lab.b.class("AppEvents", APP, &["_listeners", "_listeners"]);
        let events = lab.b.object(events_class, 24, Value::None, &[list, 0]);
        lab.b.static_field("instance", events);
        lab.b.hash(leaked, 777);
        leaked
    }

    #[test]
    fn profile_unmounted_state_with_the_jank_lab_path() {
        let mut lab = Lab::new(false, NOTIFIER);
        jank_lab_leak(&mut lab);
        let (_d, a) = lab.run(Some("3.47.9"), Some("jank_lab"));
        let s = &a.summary;
        assert_eq!(s.checks.states, Check::Done);
        assert_eq!(s.checks.elements, Check::Done);
        assert_eq!(s.findings_total, 1);
        let f = &s.findings[0];
        assert_eq!(f.kind, LeakKind::UnmountedState);
        let classes = names(&a, f);
        assert_eq!(
            classes[classes.len() - 5..],
            [
                "AppEvents",
                "_List",
                "_Closure",
                "Context",
                "_LeakyScreenState"
            ]
        );
        assert_eq!(f.path[0].node, 0);
        assert_eq!(f.path[0].edge, Edge::RootSlice("isolate".into()));
        assert_eq!(f.path[1].edge, Edge::Static("instance".into()));
        let culprit = &f.path[f.culprit.unwrap() as usize];
        assert_eq!(s.classes[culprit.class as usize].name.as_ref(), "AppEvents");
        assert_eq!(culprit.edge, Edge::Field("_listeners".into()));
        assert!(
            f.keeps
                .iter()
                .any(|(c, _)| s.classes[*c as usize].name.as_ref() == "TextEditingController")
        );
        assert_eq!(f.retained, a.index.retained(f.node));
        assert_eq!(f.path.last().unwrap().node, f.node);
        assert_eq!(
            path_text(s, &f.path),
            "Root → static instance → AppEvents._listeners → _List[0] → _Closure (closure context) → Context (captured 0) → _LeakyScreenState"
        );
        // The tracked hash maps back to the leaked node.
        let t = s.tracked.iter().find(|t| t.class == f.class).unwrap();
        assert_eq!((&*t.hashes, &*t.nodes), (&[777u32][..], &[f.node][..]));
        // Profile mode: the notifier check cannot run, and there is no HTTP recording.
        assert_eq!(s.checks.notifiers, Check::Undecidable("profile".into()));
        assert!(s.http.is_none());
    }

    #[test]
    fn debug_disposed_notifier_via_a_static_list() {
        let mut lab = Lab::new(true, NOTIFIER);
        let scroll = lab.b.class(
            "ScrollController",
            "package:flutter/src/widgets/scroll_controller.dart",
            &[
                "_count",
                "_listeners",
                "_notificationCallStackDepth",
                "_debugDisposed",
                "_initialScrollOffset",
            ],
        );
        let disposed = lab
            .b
            .object(scroll, 40, Value::None, &[0, 0, 0, lab.yes, 0]);
        let alive = lab.b.object(scroll, 40, Value::None, &[0, 0, 0, lab.no, 0]);
        let growable = lab.b.class(
            "_GrowableList",
            "dart:core",
            &["type_arguments", "length_", "data_"],
        );
        let data = lab
            .b
            .object(lab.list, 32, Value::Length(2), &[0, 0, disposed, alive]);
        let g = lab.b.object(growable, 24, Value::None, &[0, 0, data]);
        lab.b.static_field("controllers", g);
        let (_d, a) = lab.run(Some("3.47.9"), Some("jank_lab"));
        let s = &a.summary;
        assert_eq!(s.checks.notifiers, Check::Done);
        assert_eq!(s.findings.len(), 1);
        let f = &s.findings[0];
        assert_eq!(f.kind, LeakKind::DisposedNotifier);
        let culprit = &f.path[f.culprit.unwrap() as usize];
        assert_eq!(culprit.edge, Edge::Static("controllers".into()));
        assert_eq!(
            names(&a, f)[2..],
            ["_GrowableList", "_List", "ScrollController"]
        );
    }

    #[test]
    fn profile_notifiers_are_not_reported() {
        let mut lab = Lab::new(false, NOTIFIER);
        let sentinel = lab.b.object(lab.list, 16, Value::Length(0), &[0, 0]);
        let n = lab
            .b
            .object(lab.notifier, 32, Value::None, &[0, sentinel, 0]);
        lab.b.static_field("notifier", n);
        let (_d, a) = lab.run(Some("3.47.9"), None);
        assert_eq!(
            a.summary.checks.notifiers,
            Check::Undecidable("profile".into())
        );
        assert!(a.summary.findings.is_empty());
        assert_eq!(a.summary.checks.states, Check::Done);
    }

    #[test]
    fn defunct_element_by_name_or_null_widget() {
        let mut lab = Lab::new(false, NOTIFIER);
        let class = lab.element;
        let live_name = lab.text("active");
        let dead_name = lab.text("defunct");
        let active = lab
            .b
            .object(lab.lifecycle, 16, Value::None, &[0, live_name]);
        let defunct = lab
            .b
            .object(lab.lifecycle, 16, Value::None, &[0, dead_name]);
        let widget = lab.b.object(lab.list, 16, Value::Length(0), &[0, 0]);
        let by_name = lab.b.object(class, 40, Value::None, &[0, defunct, widget]);
        let by_widget = lab.b.object(class, 40, Value::None, &[0, active, lab.null]);
        let healthy = lab.b.object(class, 40, Value::None, &[0, active, widget]);
        lab.list_static("elements", &[by_name, by_widget, healthy]);
        let (_d, a) = lab.run(Some("3.47.9"), None);
        let s = &a.summary;
        assert_eq!(s.checks.elements, Check::Done);
        assert_eq!(s.findings_total, 2);
        assert!(
            s.findings
                .iter()
                .all(|f| f.kind == LeakKind::DefunctElement)
        );
        assert_eq!(s.tracked.len(), 0, "no identity hashes were written");
    }

    #[test]
    fn garbage_states_are_not_leaks() {
        let mut lab = Lab::new(false, NOTIFIER);
        let class = lab.user_state_class("_GarbageState");
        let garbage = lab.b.object(class, 32, Value::None, &[0, 0, lab.null, 0]);
        let _ = garbage;
        let (_d, a) = lab.run(Some("3.47.9"), Some("jank_lab"));
        assert!(a.summary.findings.is_empty());
        assert_eq!(a.summary.findings_total, 0);
        assert_eq!(a.summary.checks.states, Check::Done);
    }

    #[test]
    fn shadowed_private_field_uses_the_first() {
        let mut lab = Lab::new(true, NOTIFIER);
        let class = lab.b.class(
            "Shadowing",
            APP,
            &[
                "_count",
                "_listeners",
                "_notificationCallStackDepth",
                "_debugDisposed",
                "_debugDisposed",
            ],
        );
        let first_true = lab
            .b
            .object(class, 40, Value::None, &[0, 0, 0, lab.yes, lab.no]);
        let second_true = lab
            .b
            .object(class, 40, Value::None, &[0, 0, 0, lab.no, lab.yes]);
        lab.list_static("notifiers", &[first_true, second_true]);
        let (_d, a) = lab.run(None, None);
        assert_eq!(a.summary.findings_total, 1);
        let f = &a.summary.findings[0];
        assert_eq!(f.kind, LeakKind::DisposedNotifier);
        // Of the two, the one reachable first in the list: element 0.
        assert_eq!(f.path[f.path.len() - 2].edge, Edge::Element(0));
    }

    fn debug_leak_with(uri: &str, version: Option<&str>) -> Analysis {
        let mut lab = Lab::new(true, uri);
        let sub = lab.b.class(
            "ValueNotifier",
            uri,
            &[
                "_count",
                "_listeners",
                "_notificationCallStackDepth",
                "_debugDisposed",
                "_value",
            ],
        );
        let n = lab.b.object(sub, 40, Value::None, &[0, 0, 0, lab.yes, 0]);
        lab.list_static("notifiers", &[n]);
        let (dir, a) = lab.run(version, None);
        std::mem::forget(dir);
        a
    }

    #[test]
    fn listen_uri_on_3_50() {
        for version in [Some("3.50.0"), Some("3.47.9")] {
            let a = debug_leak_with(LISTEN, version);
            assert_eq!(a.summary.checks.notifiers, Check::Done, "{version:?}");
            assert_eq!(a.summary.findings.len(), 1, "{version:?}");
        }
        let a = debug_leak_with(NOTIFIER, Some("3.50.0"));
        assert_eq!(a.summary.findings.len(), 1, "the other row is tried");
    }

    #[test]
    fn unknown_version_tries_every_row() {
        for version in [None, Some("main"), Some("")] {
            for uri in [LISTEN, NOTIFIER] {
                let a = debug_leak_with(uri, version);
                assert_eq!(a.summary.findings.len(), 1, "{version:?} {uri}");
            }
        }
    }

    #[test]
    fn no_flutter_classes_marks_not_found() {
        let mut b = SnapshotBuilder::new();
        let plain = b.class("Plain", APP, &["a"]);
        let o = b.object(plain, 16, Value::None, &[0]);
        b.static_field("x", o);
        let (_d, a) = run_snapshot(b, Some("3.47.9"), Some("jank_lab"));
        let c = &a.summary.checks;
        assert_eq!(
            (&c.states, &c.elements, &c.notifiers),
            (&Check::NotFound, &Check::NotFound, &Check::NotFound)
        );
        assert!(a.summary.findings.is_empty());
        assert!(
            a.summary
                .classes
                .iter()
                .any(|c| c.user && c.name.as_ref() == "Plain")
        );
    }

    #[test]
    fn missing_field_marks_undecidable() {
        let mut b = SnapshotBuilder::new();
        b.class("State", FW, &["type_arguments", "_widget"]);
        b.class("Element", FW, &["_parent"]);
        let (_d, a) = run_snapshot(b, None, None);
        assert_eq!(
            a.summary.checks.states,
            Check::Undecidable("_element".into())
        );
        assert_eq!(
            a.summary.checks.elements,
            Check::Undecidable("_widget, _lifecycleState".into())
        );
        assert_eq!(a.summary.checks.notifiers, Check::NotFound);
    }

    #[test]
    fn field_index_past_ref_count_is_skipped() {
        let mut lab = Lab::new(false, NOTIFIER);
        // The class table says `_element` is ref 2, but the object only wrote one ref.
        let class = lab.user_state_class("_Short");
        let short = lab.b.object(class, 16, Value::None, &[0]);
        let notifier = lab.b.object(lab.notifier, 16, Value::None, &[]);
        lab.list_static("held", &[short, notifier]);
        let (_d, a) = lab.run(None, Some("jank_lab"));
        assert!(a.summary.findings.is_empty());
    }

    fn graph_with_classes() -> Graph {
        let mut lab = Lab::new(false, NOTIFIER);
        let slice = lab.b.object(3, 0, Value::Name("class table".into()), &[]);
        let _ = slice;
        let bytes = encode_full(&lab.b.finish());
        parse(
            bytes.as_slice(),
            bytes.len() as u64,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap()
    }

    #[test]
    fn edge_label_cases() {
        let g = graph_with_classes();
        // From the root: a named slice, the isolate object, and a slice without a name.
        assert_eq!(
            edge(&g, 1, 1, Some("class table")),
            Edge::RootSlice("class table".into())
        );
        assert_eq!(edge(&g, 1, 3, None), Edge::RootSlice("isolate".into()));
        assert_eq!(edge(&g, 1, 1, None), Edge::RootSlice("Root slice".into()));
        // From the isolate object: the static's name.
        let mut b = SnapshotBuilder::new();
        let t = b.object(2, 0, Value::None, &[]);
        b.static_field("instance", t);
        let bytes = encode_full(&b.finish());
        let sg = parse(
            bytes.as_slice(),
            bytes.len() as u64,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        assert_eq!(edge(&sg, 5, 0, None), Edge::Static("instance".into()));
        assert_eq!(edge(&sg, 5, 9, None), Edge::Internal(9));
        // Lists, closures and contexts.
        assert_eq!(
            label(Holder::Class, "_List", Some("length_"), 1, ""),
            Edge::Field("length_".into())
        );
        assert_eq!(label(Holder::Class, "_List", None, 2, ""), Edge::Element(0));
        assert_eq!(
            label(Holder::Class, "_ImmutableList", None, 7, ""),
            Edge::Element(5)
        );
        assert_eq!(
            label(Holder::Class, "_Closure", None, 3, ""),
            Edge::ClosureContext
        );
        assert_eq!(
            label(Holder::Class, "_Closure", Some("function"), 2, ""),
            Edge::Field("function".into())
        );
        assert_eq!(
            label(Holder::Class, "Context", Some("parent_"), 0, ""),
            Edge::Field("parent_".into())
        );
        assert_eq!(
            label(Holder::Class, "Context", None, 1, ""),
            Edge::Captured(0)
        );
        assert_eq!(
            label(Holder::Class, "Context", None, 4, ""),
            Edge::Captured(3)
        );
        assert_eq!(
            label(Holder::Class, "Thing", Some("_text"), 2, ""),
            Edge::Field("_text".into())
        );
        assert_eq!(
            label(Holder::Class, "Thing", None, 6, ""),
            Edge::Internal(6)
        );
    }

    #[test]
    fn classify_follows_the_field_prefix() {
        let mut lab = Lab::new(false, NOTIFIER);
        let sub = lab.user_state_class("_S");
        let unrelated = lab.b.class("Other", APP, &["_widget", "_element"]);
        let sub_n = lab.b.class(
            "MyNotifier",
            APP,
            &[
                "_count",
                "_listeners",
                "_notificationCallStackDepth",
                "extra",
            ],
        );
        let bytes = encode_full(&lab.b.finish());
        let g = parse(
            bytes.as_slice(),
            bytes.len() as u64,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        let kinds = classify(&g, sdk::rows_for(Some("3.47.9"))[0]);
        assert_eq!(kinds[sub as usize], Some(LifecycleKind::State));
        assert_eq!(kinds[unrelated as usize], None);
        assert_eq!(kinds[sub_n as usize], Some(LifecycleKind::Notifier));
        assert_eq!(kinds[0], None);
        // The listen row does not know the foundation URI's notifier.
        let listen = classify(&g, sdk::rows_for(Some("3.50.0"))[0]);
        assert_eq!(listen[sub_n as usize], None);
        assert!(is_user_class("package:jank_lab/a.dart", Some("jank_lab")));
        assert!(!is_user_class("package:jank_lab2/a.dart", Some("jank_lab")));
        assert!(!is_user_class("package:flutter/a.dart", Some("jank_lab")));
        assert!(is_user_class("file:///Users/x/app/lib/a.dart", None));
        assert!(!is_user_class(
            "file:///Users/x/.pub-cache/hosted/a.dart",
            None
        ));
    }

    #[test]
    fn path_text_marks_a_truncated_path() {
        let mut lab = Lab::new(false, NOTIFIER);
        jank_lab_leak(&mut lab);
        let (_d, a) = lab.run(None, Some("jank_lab"));
        let f = &a.summary.findings[0];
        assert_eq!(
            path_text(&a.summary, &f.path[3..]),
            "… → _List[0] → _Closure (closure context) → Context (captured 0) → _LeakyScreenState"
        );
        assert_eq!(path_text(&a.summary, &[]), "");
    }

    #[test]
    fn http_held_sums_both_sources() {
        let mut lab = Lab::new(false, NOTIFIER);
        let data = lab.b.class(
            "_HttpProfileData",
            "dart:_http",
            &["requestBody", "responseBody"],
        );
        let body = |lab: &mut Lab, size| lab.b.object(lab.list, size, Value::Length(0), &[0, 0]);
        let (b1, b2, b3, b4) = (
            body(&mut lab, 50),
            body(&mut lab, 50),
            body(&mut lab, 50),
            body(&mut lab, 50),
        );
        let one = lab.b.object(data, 100, Value::None, &[b1, b2]);
        let two = lab.b.object(data, 100, Value::None, &[b3, b4]);
        let map = lab
            .b
            .class("_Map", "dart:core", &["type_arguments", "length_", "data_"]);
        let m = lab.b.object(map, 20, Value::None, &[0, 0, one, two]);
        lab.b.static_field("_profile", m);
        // The `package:http_profile` store: a growable list of maps.
        let growable = lab.b.class(
            "_GrowableList",
            "dart:core",
            &["type_arguments", "length_", "data_"],
        );
        let entry = lab.b.object(lab.list, 60, Value::Length(1), &[0, 0, 0]);
        let g = lab.b.object(growable, 40, Value::None, &[0, 0, entry]);
        lab.b.static_field("_developerProfilingData", g);
        let (_d, a) = lab.run(Some("3.47.9"), None);
        let http = a.summary.http.expect("an HTTP recording");
        assert_eq!(http.requests, 2);
        assert_eq!(http.bytes, 2 * (100 + 2 * 50) + 40 + 60);
    }

    #[test]
    fn findings_are_capped_with_a_total() {
        let mut lab = Lab::new(false, NOTIFIER);
        let class = lab.user_state_class("_Leaked");
        let states: Vec<u32> = (0..250).map(|_| lab.state_of(class, lab.null)).collect();
        lab.list_static("states", &states);
        let (_d, a) = lab.run(None, Some("jank_lab"));
        assert_eq!(a.summary.findings_total, 250);
        assert_eq!(a.summary.findings.len(), MAX_FINDINGS);
        assert!(
            a.summary
                .findings
                .iter()
                .all(|f| f.path.last().unwrap().node == f.node)
        );
    }

    #[test]
    fn a_deep_chain_has_a_capped_path() {
        let mut lab = Lab::new(false, NOTIFIER);
        let link = lab.b.class("Link", APP, &["next"]);
        let class = lab.user_state_class("_Deep");
        let mut at = lab.state_of(class, lab.null);
        for _ in 0..500 {
            at = lab.b.object(link, 16, Value::None, &[at]);
        }
        lab.b.static_field("head", at);
        let (_d, a) = lab.run(None, Some("jank_lab"));
        let f = &a.summary.findings[0];
        assert_eq!(f.path.len(), MAX_PATH_HOPS);
        assert_ne!(f.path[0].node, 0);
        assert!(path_text(&a.summary, &f.path).starts_with("… → Link.next"));
    }

    /// An element class with the given fields, and optionally the lifecycle enum.
    fn element_snapshot(fields: &[&str], enum_class: bool) -> Analysis {
        let mut b = SnapshotBuilder::new();
        let nul = b.class("Null", "dart:core", &[]);
        let string = b.class("_OneByteString", "dart:core", &[]);
        let list = b.class("_List", "dart:core", &["type_arguments", "length_"]);
        let element = b.class("Element", FW, fields);
        let null = b.object(nul, 0, Value::Null, &[]);
        let widget = b.object(list, 16, Value::Length(0), &[0, 0]);
        let mut items = Vec::new();
        let refs = |life: u32, w: u32| -> Vec<u32> {
            fields
                .iter()
                .map(|f| match *f {
                    "_lifecycleState" => life,
                    "_widget" => w,
                    _ => 0,
                })
                .collect()
        };
        if enum_class {
            let lc = b.class("_ElementLifecycle", FW, &["index", "_name"]);
            let text = |b: &mut SnapshotBuilder, s: &str| {
                b.object(
                    string,
                    16,
                    Value::Latin1 {
                        len: s.len() as u32,
                        bytes: s.as_bytes().to_vec(),
                    },
                    &[],
                )
            };
            let (dn, an) = (text(&mut b, "defunct"), text(&mut b, "active"));
            let defunct = b.object(lc, 16, Value::None, &[0, dn]);
            let active = b.object(lc, 16, Value::None, &[0, an]);
            items.push(b.object(element, 40, Value::None, &refs(defunct, widget)));
            items.push(b.object(element, 40, Value::None, &refs(active, widget)));
            items.push(b.object(element, 40, Value::None, &refs(active, null)));
        } else {
            items.push(b.object(element, 40, Value::None, &refs(0, widget)));
            items.push(b.object(element, 40, Value::None, &refs(0, null)));
        }
        let mut held_refs = vec![0, 0];
        held_refs.extend(&items);
        let held = b.object(list, 32, Value::Length(items.len() as u64), &held_refs);
        b.static_field("elements", held);
        let (dir, a) = run_snapshot(b, None, None);
        std::mem::forget(dir);
        a
    }

    #[test]
    fn element_check_runs_on_the_null_widget_alone() {
        // The enum class is missing, but `_widget` exists.
        let a = element_snapshot(&["_parent", "_lifecycleState", "_widget"], false);
        assert_eq!(a.summary.checks.elements, Check::Done);
        assert_eq!(a.summary.findings_total, 1, "only the null-widget element");
    }

    #[test]
    fn element_check_runs_on_the_enum_name_alone() {
        // `_widget` is missing, but the enum works.
        let a = element_snapshot(&["_parent", "_lifecycleState"], true);
        assert_eq!(a.summary.checks.elements, Check::Done);
        assert_eq!(
            a.summary.findings_total, 1,
            "only the defunct-by-name element"
        );
        // With both signals, each unmounted element is found and the healthy one is not.
        let both = element_snapshot(&["_parent", "_lifecycleState", "_widget"], true);
        assert_eq!(both.summary.findings_total, 2);
    }

    #[test]
    fn element_check_is_undecidable_when_neither_signal_runs() {
        // No `_widget`, and the enum class is missing.
        let a = element_snapshot(&["_parent", "_lifecycleState"], false);
        assert_eq!(
            a.summary.checks.elements,
            Check::Undecidable("_widget, _ElementLifecycle".into())
        );
        assert_eq!(a.summary.findings_total, 0);
    }

    #[test]
    fn a_missing_snapshot_file_is_an_io_error_not_a_clean_scan() {
        let dir = tempfile::tempdir().unwrap();
        let input = AnalyzeInput {
            file: Arc::new(HeapFile::new(dir.path().join("gone.dartheap"))),
            flutter_version: None,
            mode: Mode::Profile,
            root_package: None,
        };
        let got = analyze(&input, &AtomicBool::new(false), &mut |_| {});
        assert!(matches!(got, Err(HeapError::Io(_))), "{:?}", got.err());
    }

    #[test]
    fn a_failed_read_during_the_scan_fails_it() {
        // The scan reads the enum's names from the file; a file that cannot serve them is an
        // error, never "not defunct".
        let mut lab = Lab::new(false, NOTIFIER);
        let name = lab.text("defunct");
        lab.b.object(lab.lifecycle, 16, Value::None, &[0, name]);
        let bytes = encode_full(&lab.b.finish());
        let g = parse(
            bytes.as_slice(),
            bytes.len() as u64,
            &AtomicBool::new(false),
            &mut |_| {},
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.dartheap");
        std::fs::write(&path, b"").unwrap();
        let file = File::open(&path).unwrap();
        let rows = sdk::rows_for(None);
        let retainers = vec![1u32; g.object_count() + 1];
        let got = scan(&g, &retainers, &file, &rows, None, &AtomicBool::new(false));
        assert!(got.is_err(), "{:?}", got.err());
    }

    #[test]
    fn the_cap_keeps_the_largest_findings() {
        let mut lab = Lab::new(false, NOTIFIER);
        let class = lab.user_state_class("_Leaked");
        // 300 unmounted states whose retained size differs: `_text` has a varied size.
        let sizes: Vec<u64> = (0..300u64).map(|i| 16 + 8 * ((i * 37) % 300)).collect();
        let states: Vec<u32> = sizes
            .iter()
            .map(|&size| {
                let text = lab.text("t");
                lab.b.set_shallow(text, size);
                let null = lab.null;
                lab.b.object(class, 32, Value::None, &[0, 0, null, text])
            })
            .collect();
        lab.list_static("states", &states);
        let (_d, a) = lab.run(None, Some("jank_lab"));
        let s = &a.summary;
        assert_eq!(s.findings_total, 300);
        assert_eq!(s.findings.len(), MAX_FINDINGS);
        let got: Vec<u64> = s.findings.iter().map(|f| f.retained).collect();
        let mut want: Vec<u64> = sizes.iter().map(|z| z + 32).collect();
        want.sort_unstable_by(|a, b| b.cmp(a));
        want.truncate(MAX_FINDINGS);
        assert_eq!(got, want, "the 200 largest, descending");
        assert!(
            s.findings
                .iter()
                .all(|f| f.path[0].node == 0 && f.path.len() == 4)
        );
    }
}
