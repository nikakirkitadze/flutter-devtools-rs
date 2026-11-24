//! Synthetic Flutter-shaped heap snapshots for the budget tests and the benches.
//!
//! [`flutter_heap`] writes an encoded snapshot of the fixed layout (H10) straight from compact
//! arrays, never through `ObjectRecord`s, so 5,000,000 objects need about 300 MB while
//! generating. [`replicate`] concatenates remapped copies of a decoded real snapshot.
//!
//! The shape: a read-only object that refers to a tenth of the objects; about 800 statics; a
//! 200-deep element spine with `_parent` back-pointers, whose nodes hold lists of element
//! subtrees; States pointing back at their element; listener lists of closures with contexts;
//! maps; a list of 100,000 items; shared strings with high fan-in; negative Mints; and about a
//! third of the objects unreachable, in clusters that point into the live graph.

use super::encode::{SnapshotBuilder, put_sleb, put_str, put_uleb};
use super::format::{ExternalDef, FullSnapshot, HeapHeader, ObjectRecord};
use fdt_store::run::Mode;

const FW: &str = "package:flutter/src/widgets/framework.dart";
const NOTIFIER: &str = "package:flutter/src/foundation/change_notifier.dart";
const APP: &str = "package:synth_app/main.dart";
const MODEL: &str = "package:synth_app/model.dart";

/// The root package to analyse a synthetic snapshot with.
pub const ROOT_PACKAGE: &str = "synth_app";
/// A Flutter version whose SDK row the synthetic snapshot matches.
pub const FLUTTER_VERSION: &str = "3.47.9";
/// The smallest snapshot [`flutter_heap`] builds.
pub const MIN_OBJECTS: u32 = 20_000;

const STATICS: usize = 800;
const BUCKETS: usize = 600;
const SPINE: usize = 200;
const BIG_LIST: usize = 100_000;
/// Objects the closing step creates: a `_GrowableList` and its `_List` per bucket, and a
/// `_List` per spine node.
const TAIL: u32 = (BUCKETS * 2 + SPINE) as u32;
/// Fraction of the objects that are reachable (the rest is garbage).
const LIVE_SHARE: f64 = 0.66;
/// Fraction of the objects the read-only object refers to.
const READ_ONLY_SHARE: f64 = 0.10;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// Small values far more often than large ones: a high fan-in for the first few.
    fn skewed(&mut self, n: usize) -> usize {
        let n = n as u64;
        (self.below(n) * self.below(n) / n) as usize
    }
}

// Value tags.
const NONE: u8 = 0;
const NULL: u8 = 1;
const FALSE: u8 = 2;
const TRUE: u8 = 3;
const SMI: u8 = 4;
const MINT: u8 = 5;
const SHARED: u8 = 6;
const UNIQUE: u8 = 7;
const LENGTH: u8 = 8;
const NAME: u8 = 9;

/// Class ids.
#[derive(Default)]
struct Classes {
    null: u32,
    boolean: u32,
    smi: u32,
    mint: u32,
    string: u32,
    list: u32,
    glist: u32,
    map: u32,
    closure: u32,
    context: u32,
    sentinel: u32,
    widget: u32,
    element: u32,
    lifecycle: u32,
    notifier: u32,
    state: u32,
    leaky_state: u32,
    /// By arity: 3, 5, 8, 12.
    items: [u32; 4],
}

const ARITIES: [usize; 4] = [3, 5, 8, 12];

struct Gen {
    rng: Rng,
    debug: bool,
    c: Classes,
    class: Vec<u32>,
    shallow: Vec<u32>,
    tag: Vec<u8>,
    payload: Vec<i64>,
    hash: Vec<u32>,
    first: Vec<u32>,
    refs: Vec<u32>,
    names: Vec<String>,
    // Pools, referenced from everywhere.
    smis: Vec<u32>,
    mints: Vec<u32>,
    strings: Vec<u32>,
    null: u32,
    yes: u32,
    no: u32,
    active: u32,
    /// Where the bulk starts.
    lo: u32,
    /// Items that later objects may refer to (a sample of them).
    targets: Vec<u32>,
    /// Once the live objects are done: how many `targets` are live.
    frozen: Option<usize>,
    spine: Vec<u32>,
    buckets: Vec<Vec<u32>>,
    spine_buckets: Vec<Vec<u32>>,
    big_cap: usize,
}

impl Gen {
    fn next_id(&self) -> u32 {
        self.class.len() as u32 + 1
    }

    fn obj(&mut self, class: u32, shallow: u32, tag: u8, payload: i64, refs: &[u32]) -> u32 {
        self.first.push(self.refs.len() as u32);
        self.refs.extend_from_slice(refs);
        self.class.push(class);
        self.shallow.push(shallow);
        self.tag.push(tag);
        self.payload.push(payload);
        self.hash.push(0);
        self.class.len() as u32
    }

    fn patch(&mut self, o: u32, pos: usize, v: u32) {
        let at = self.first[o as usize - 1] as usize + pos;
        self.refs[at] = v;
    }

    fn set_hash(&mut self, o: u32) {
        self.hash[o as usize - 1] = (self.rng.next() as u32) | 1;
    }

    fn unique_string(&mut self) -> u32 {
        let id = self.next_id();
        self.obj(self.c.string, 32, UNIQUE, i64::from(id), &[])
    }

    fn name_object(&mut self, class: u32, name: &str) -> u32 {
        self.names.push(name.to_owned());
        self.obj(class, 0, NAME, self.names.len() as i64 - 1, &[])
    }

    /// An object of a pool: a Smi, a shared string, a Mint or a constant.
    fn leaf(&mut self) -> u32 {
        match self.rng.below(20) {
            0..=9 => self.smis[self.rng.below(self.smis.len() as u64) as usize],
            10..=14 => {
                let i = self.rng.skewed(self.strings.len());
                self.strings[i]
            }
            15..=16 => self.mints[self.rng.below(self.mints.len() as u64) as usize],
            17 => self.null,
            18 => self.yes,
            _ => self.no,
        }
    }

    /// A random data object (an item) created earlier, live ones only once the live end is
    /// fixed; half of the time a recent one. Never an element: a reference to one would give
    /// its `_parent` a second path and flatten the dominator tree.
    fn earlier(&mut self) -> u32 {
        let len = self.frozen.unwrap_or(self.targets.len());
        if len == 0 {
            return self.leaf();
        }
        let from = if self.rng.below(2) == 0 && len > 1_000 {
            len - 500
        } else {
            0
        };
        self.targets[from + self.rng.below((len - from) as u64) as usize]
    }

    /// One field of a data object: a pool object, an earlier object or unboxed.
    fn field(&mut self) -> u32 {
        match self.rng.below(10) {
            0..=3 => self.leaf(),
            4 => 0,
            _ => self.earlier(),
        }
    }

    fn item(&mut self, arity_ix: usize, first: &[u32]) -> u32 {
        let arity = ARITIES[arity_ix];
        let mut refs: Vec<u32> = first.to_vec();
        while refs.len() < arity {
            let f = self.field();
            refs.push(f);
        }
        let o = self.obj(
            self.c.items[arity_ix],
            16 + 8 * arity as u32,
            NONE,
            0,
            &refs,
        );
        if o.is_multiple_of(16) {
            self.set_hash(o);
        }
        if o.is_multiple_of(4) && self.frozen.is_none() {
            self.targets.push(o);
        }
        o
    }

    fn list(&mut self, items: &[u32]) -> u32 {
        let mut refs = vec![0, 0];
        refs.extend_from_slice(items);
        self.obj(
            self.c.list,
            16 + 8 * items.len() as u32,
            LENGTH,
            items.len() as i64,
            &refs,
        )
    }

    fn data_unit(&mut self) -> u32 {
        let kids = 2 + self.rng.below(3) as usize;
        let mut heads = Vec::with_capacity(kids);
        for _ in 0..kids {
            let s = self.unique_string();
            let ix = 2 + self.rng.below(2) as usize;
            let k = self.item(ix, &[s]);
            heads.push(k);
        }
        self.item(3, &heads)
    }

    fn map_unit(&mut self) -> u32 {
        let pairs = 2 + self.rng.below(5) as usize;
        let mut entries = Vec::with_capacity(pairs * 2);
        for _ in 0..pairs {
            let i = self.rng.skewed(self.strings.len());
            entries.push(self.strings[i]);
            let v = self.field();
            entries.push(if v == 0 { self.leaf() } else { v });
        }
        let data = self.list(&entries);
        self.obj(self.c.map, 32, NONE, 0, &[0, 0, data])
    }

    fn closure(&mut self) -> u32 {
        let parent = if self.rng.below(2) == 0 {
            0
        } else {
            self.earlier()
        };
        let vars = [self.field(), self.field(), self.field()];
        let ctx = self.obj(
            self.c.context,
            16 + 8 * 4,
            NONE,
            0,
            &[parent, vars[0], vars[1], vars[2]],
        );
        self.obj(self.c.closure, 48, NONE, 0, &[0, 0, 0, ctx])
    }

    fn notifier_unit(&mut self) -> u32 {
        let n = 1 + self.rng.below(3) as usize;
        let closures: Vec<u32> = (0..n).map(|_| self.closure()).collect();
        let listeners = self.list(&closures);
        let value = self.field();
        let mut refs = vec![0, listeners, 0];
        if self.debug {
            refs.push(self.no);
        }
        refs.push(value);
        self.obj(self.c.notifier, 56, NONE, 0, &refs)
    }

    fn state_refs(&mut self, widget: u32, element: u32, text: u32) -> Vec<u32> {
        let mut refs = vec![0, widget, element];
        if self.debug {
            refs.push(0);
        }
        refs.push(text);
        refs
    }

    /// One element of the tree, its widget and its State. The element's `_parent` is `parent`;
    /// `_child` and `_children` stay unset.
    fn element(&mut self, parent: u32) -> (u32, u32) {
        let key = self.leaf();
        let widget = self.obj(self.c.widget, 24, NONE, 0, &[key, 0]);
        let child = self.field();
        self.patch(widget, 1, child);
        let element = self.obj(
            self.c.element,
            64,
            NONE,
            0,
            &[parent, self.active, widget, 0, 0, 0],
        );
        let text = self.unique_string();
        let refs = self.state_refs(widget, element, text);
        let state = self.obj(self.c.state, 48, NONE, 0, &refs);
        self.patch(element, 4, state);
        self.set_hash(element);
        self.set_hash(state);
        (element, state)
    }

    /// A chain of 1-5 elements hanging off a spine node.
    fn element_unit(&mut self, node: usize) -> u32 {
        let parent = self.spine[node];
        let depth = 1 + self.rng.below(5);
        let (head, _) = self.element(parent);
        let mut at = head;
        for _ in 1..depth {
            let (next, _) = self.element(at);
            self.patch(at, 3, next);
            at = next;
        }
        head
    }

    /// Generates units until the next id passes `until`. Live units are attached to a bucket.
    fn units(&mut self, until: u32, live: bool) {
        while self.next_id() <= until {
            let roll = self.rng.below(100);
            // An element's subtree is held by the spine node it points back at; any other
            // spine node would be a shortcut that flattens the dominator tree.
            let node = self.rng.below(SPINE as u64) as usize;
            let head = match roll {
                0..=57 => self.data_unit(),
                58..=71 => self.map_unit(),
                72..=78 => self.notifier_unit(),
                79..=81 => self.closure(),
                _ => self.element_unit(node),
            };
            if !live {
                continue;
            }
            if roll >= 82 {
                self.spine_buckets[node].push(head);
            } else if roll < 58 && self.buckets[0].len() < self.big_cap {
                self.buckets[0].push(head);
            } else {
                let b = 1 + self.rng.below(BUCKETS as u64 - 1) as usize;
                self.buckets[b].push(head);
            }
        }
    }
}

fn classes(debug: bool) -> (Vec<super::format::ClassDef>, Classes) {
    let mut b = SnapshotBuilder::new();
    let mut c = Classes {
        null: b.class("Null", "dart:core", &[]),
        boolean: b.class("bool", "dart:core", &[]),
        smi: b.class("_Smi", "dart:core", &[]),
        mint: b.class("_Mint", "dart:core", &[]),
        string: b.class("_OneByteString", "dart:core", &[]),
        list: b.class("_List", "dart:core", &["type_arguments", "length_"]),
        glist: b.class(
            "_GrowableList",
            "dart:core",
            &["type_arguments", "length_", "data_"],
        ),
        map: b.class("_Map", "dart:core", &["type_arguments", "length_", "data_"]),
        closure: b.class(
            "_Closure",
            "dart:core",
            &[
                "instantiator_type_arguments",
                "function_type_arguments",
                "function",
            ],
        ),
        context: b.class("Context", "dart:core", &["parent_"]),
        sentinel: b.class("Sentinel", "dart:core", &[]),
        widget: b.class("Widget", FW, &["key", "_child"]),
        ..Classes::default()
    };
    let mut state = vec!["type_arguments", "_widget", "_element"];
    let mut notifier = vec!["_count", "_listeners", "_notificationCallStackDepth"];
    if debug {
        state.push("_debugLifecycleState");
        notifier.push("_debugDisposed");
    }
    b.class("State", FW, &state);
    b.class("Element", FW, &["_parent", "_lifecycleState", "_widget"]);
    b.class("ChangeNotifier", NOTIFIER, &notifier);
    c.lifecycle = b.class("_ElementLifecycle", FW, &["index", "_name"]);
    c.element = b.class(
        "StatefulElement",
        FW,
        &[
            "_parent",
            "_lifecycleState",
            "_widget",
            "_child",
            "_state",
            "_children",
        ],
    );
    let mut s = state.clone();
    s.push("_text");
    c.state = b.class("_HomeState", APP, &s);
    c.leaky_state = b.class("_LeakyState", APP, &s);
    let mut n = notifier.clone();
    n.push("value");
    c.notifier = b.class("AppNotifier", APP, &n);
    for (i, arity) in ARITIES.iter().enumerate() {
        let fields: Vec<String> = (0..*arity).map(|f| format!("a{f}")).collect();
        let refs: Vec<&str> = fields.iter().map(String::as_str).collect();
        c.items[i] = b.class(&format!("Item{arity}"), MODEL, &refs);
    }
    let mut statics = vec![
        "appNotifier".to_owned(),
        "leakedElements".to_owned(),
        "disposedNotifiers".to_owned(),
        "rootElement".to_owned(),
    ];
    for i in 0..BUCKETS {
        statics.push(if i == 0 {
            "bigList".to_owned()
        } else {
            format!("cache{i}")
        });
    }
    while statics.len() < STATICS {
        let i = statics.len();
        statics.push(if i % 37 == 0 {
            "instance".to_owned()
        } else {
            format!("static{i}")
        });
    }
    for name in &statics {
        b.static_field(name, 0);
    }
    (b.finish().classes, c)
}

/// Builds an encoded snapshot of `objects` objects (at least [`MIN_OBJECTS`]) in the fixed
/// layout. The same `seed` and `mode` give the same bytes.
///
/// Planted leaks, which `analyze` finds with [`ROOT_PACKAGE`] and [`FLUTTER_VERSION`]: one
/// unmounted State held by a user notifier's listener closure, one defunct Element in a static
/// list, and in `Mode::Debug` one disposed notifier in a static list.
pub fn flutter_heap(seed: u64, objects: u32, mode: Mode) -> Vec<u8> {
    assert!(
        objects >= MIN_OBJECTS,
        "a synthetic heap needs {MIN_OBJECTS} objects at least"
    );
    let debug = mode == Mode::Debug;
    let (class_defs, c) = classes(debug);
    let n = objects as usize;
    let mut g = Gen {
        rng: Rng(seed ^ 0x5EED_F1A7),
        debug,
        c,
        class: Vec::with_capacity(n),
        shallow: Vec::with_capacity(n),
        tag: Vec::with_capacity(n),
        payload: Vec::with_capacity(n),
        hash: Vec::with_capacity(n),
        first: Vec::with_capacity(n),
        refs: Vec::with_capacity(n * 21 / 4),
        names: Vec::new(),
        smis: Vec::new(),
        mints: Vec::new(),
        strings: Vec::new(),
        null: 0,
        yes: 0,
        no: 0,
        active: 0,
        lo: 0,
        targets: Vec::new(),
        frozen: None,
        spine: Vec::new(),
        buckets: vec![Vec::new(); BUCKETS],
        spine_buckets: vec![Vec::new(); SPINE],
        big_cap: BIG_LIST.min(n / 40),
    };

    // The fixed layout: 1 Root, 2 Read-Only Pages, 3..=15 root slices, 16 the isolate. The
    // read-only and isolate objects get their refs at the end.
    g.obj(1, 0, NONE, 0, &(2..=16).collect::<Vec<u32>>());
    g.obj(2, 0, NONE, 0, &[]);
    let slices = [
        "class table",
        "api state",
        "group object store",
        "object store",
        "symbol table",
        "megamorphic cache",
        "deopt table",
        "handles",
        "stack",
        "loading units",
        "finalizers",
        "weak tables",
        "dispatch table",
    ];
    for name in slices {
        g.name_object(3, name);
    }
    g.obj(4, 0, NONE, 0, &[]);

    // Pools.
    let mut pool: Vec<u32> = Vec::new();
    g.null = g.obj(g.c.null, 0, NULL, 0, &[]);
    g.yes = g.obj(g.c.boolean, 0, TRUE, 0, &[]);
    g.no = g.obj(g.c.boolean, 0, FALSE, 0, &[]);
    let sentinel = g.name_object(g.c.sentinel, "uninitialized");
    pool.extend([g.null, g.yes, g.no, sentinel]);
    let mut enum_values = Vec::new();
    for name in ["initial", "active", "inactive", "failed", "defunct"] {
        // A string whose negative payload picks its text from `names`.
        g.names.push(name.to_owned());
        let text = g.obj(g.c.string, 32, UNIQUE, -(g.names.len() as i64), &[]);
        let v = g.obj(g.c.lifecycle, 24, NONE, 0, &[0, text]);
        enum_values.push(v);
        pool.extend([text, v]);
    }
    g.active = enum_values[1];
    let defunct = enum_values[4];
    for i in 0..(n / 100).max(500) as i64 {
        let v = g.obj(g.c.smi, 0, SMI, if i % 5 == 0 { -i } else { i * 3 }, &[]);
        g.smis.push(v);
    }
    for i in 0..(n / 500).max(50) as i64 {
        let v = g.obj(g.c.mint, 16, MINT, -(1 << 40) - i * 977, &[]);
        g.mints.push(v);
    }
    for i in 0..2_000 {
        let v = g.obj(g.c.string, 40, SHARED, i, &[]);
        g.strings.push(v);
    }
    pool.extend(g.smis.iter().chain(&g.mints).chain(&g.strings));

    // The element spine: 200 deep, each node holding a list of subtrees (set at the end).
    for i in 0..SPINE {
        let parent = if i == 0 { 0 } else { g.spine[i - 1] };
        let (e, _) = g.element(parent);
        if i > 0 {
            g.patch(parent, 3, e);
        }
        g.spine.push(e);
    }

    // The planted leaks.
    let text = g.unique_string();
    let refs = [0, 0, g.null]
        .into_iter()
        .chain(debug.then_some(0))
        .chain([text])
        .collect::<Vec<u32>>();
    let leaked = g.obj(g.c.leaky_state, 48, NONE, 0, &refs);
    g.set_hash(leaked);
    let ctx = g.obj(g.c.context, 32, NONE, 0, &[0, leaked]);
    let closure = g.obj(g.c.closure, 48, NONE, 0, &[0, 0, 0, ctx]);
    let listeners = g.list(&[closure]);
    let mut refs = vec![0, listeners, 0];
    refs.extend(debug.then_some(g.no));
    refs.push(0);
    let app_notifier = g.obj(g.c.notifier, 56, NONE, 0, &refs);

    let dead_widget = g.null;
    let dead = g.obj(
        g.c.element,
        64,
        NONE,
        0,
        &[0, defunct, dead_widget, 0, 0, 0],
    );
    g.set_hash(dead);
    let dead_data = g.list(&[dead]);
    let leaked_elements = g.obj(g.c.glist, 24, NONE, 0, &[0, 0, dead_data]);
    let disposed = if debug {
        let d = g.obj(g.c.notifier, 56, NONE, 0, &[0, 0, 0, g.yes, 0]);
        let data = g.list(&[d]);
        g.obj(g.c.glist, 24, NONE, 0, &[0, 0, data])
    } else {
        sentinel
    };

    // The bulk: live objects, then garbage that points into them.
    g.lo = g.next_id();
    let live_end = (f64::from(objects) * LIVE_SHARE) as u32;
    g.units(live_end, true);
    let live_end = g.next_id() - 1;
    g.frozen = Some(g.targets.len());
    let garbage_end = objects - TAIL;
    // Stop a unit short of the end, then pad with unique strings to land exactly.
    g.units(garbage_end.saturating_sub(64), false);
    while g.next_id() <= garbage_end {
        g.unique_string();
    }

    // Close: the bucket lists, the spine's child lists, the statics, the read-only refs.
    for (round, &o) in pool.iter().enumerate() {
        g.buckets[round % BUCKETS].push(o);
    }
    let mut static_vals: Vec<u32> = vec![app_notifier, leaked_elements, disposed, g.spine[0]];
    let buckets = std::mem::take(&mut g.buckets);
    for items in &buckets {
        let data = g.list(items);
        let list = g.obj(g.c.glist, 24, NONE, 0, &[0, 0, data]);
        static_vals.push(list);
    }
    let spine_buckets = std::mem::take(&mut g.spine_buckets);
    for (i, items) in spine_buckets.iter().enumerate() {
        let list = g.list(items);
        let node = g.spine[i];
        g.patch(node, 5, list);
    }
    while static_vals.len() < STATICS {
        let i = static_vals.len();
        static_vals.push(if i.is_multiple_of(3) {
            sentinel
        } else {
            g.strings[i % g.strings.len()]
        });
    }
    debug_assert_eq!(g.next_id() - 1, objects, "the tail has a fixed size");

    let p = READ_ONLY_SHARE * f64::from(objects) / f64::from(live_end - g.lo + 1);
    let threshold = (p.min(1.0) * 4_294_967_296.0) as u64;
    let mut read_only: Vec<u32> = Vec::with_capacity(n / 8);
    for id in g.lo..=live_end {
        // Not an element or a State: either gives its subtree a second path from the root.
        let class = g.class[id as usize - 1];
        let tree = class == g.c.element || class == g.c.state;
        if g.rng.next() >> 32 < threshold && !tree {
            read_only.push(id);
        }
    }

    write(&g, &class_defs, &read_only, &static_vals)
}

fn write(
    g: &Gen,
    classes: &[super::format::ClassDef],
    read_only: &[u32],
    statics: &[u32],
) -> Vec<u8> {
    use std::fmt::Write as _;
    let n = g.class.len();
    let shallow: u64 = g.shallow.iter().map(|&s| u64::from(s)).sum();
    let mut out: Vec<u8> = Vec::with_capacity(n * 32);
    out.extend_from_slice(b"dartheap");
    put_uleb(&mut out, 0);
    put_str(&mut out, "synthetic");
    put_uleb(&mut out, shallow);
    put_uleb(&mut out, shallow);
    put_uleb(&mut out, 0);
    put_uleb(&mut out, classes.len() as u64);
    for c in classes {
        put_uleb(&mut out, c.flags);
        put_str(&mut out, &c.name);
        put_str(&mut out, &c.library_name);
        put_str(&mut out, &c.library_uri);
        put_str(&mut out, &c.reserved);
        put_uleb(&mut out, c.fields.len() as u64);
        for f in &c.fields {
            put_uleb(&mut out, f.flags);
            put_uleb(&mut out, u64::from(f.index));
            put_str(&mut out, &f.name);
            put_str(&mut out, &f.reserved);
        }
    }
    put_uleb(
        &mut out,
        (g.refs.len() + read_only.len() + statics.len()) as u64,
    );
    put_uleb(&mut out, n as u64);
    let mut text = String::new();
    for i in 0..n {
        let id = i as u32 + 1;
        put_uleb(&mut out, u64::from(g.class[i]));
        put_uleb(&mut out, u64::from(g.shallow[i]));
        let v = g.payload[i];
        match g.tag[i] {
            NONE => put_uleb(&mut out, 0),
            NULL => put_uleb(&mut out, 1),
            FALSE | TRUE => {
                put_uleb(&mut out, 2);
                put_uleb(&mut out, u64::from(g.tag[i] == TRUE));
            }
            SMI => {
                put_uleb(&mut out, 3);
                put_uleb(&mut out, v as u64);
            }
            MINT => {
                put_uleb(&mut out, 3);
                put_sleb(&mut out, v);
            }
            LENGTH => {
                put_uleb(&mut out, 7);
                put_uleb(&mut out, v as u64);
            }
            NAME => {
                put_uleb(&mut out, 8);
                put_str(&mut out, &g.names[v as usize]);
            }
            tag => {
                text.clear();
                // A negative payload names an enum value by `names`.
                if tag == UNIQUE && v < 0 {
                    text.push_str(&g.names[(-v - 1) as usize]);
                } else if tag == SHARED {
                    let _ = write!(text, "shared-string-{v}");
                } else {
                    let _ = write!(text, "s{v}");
                }
                put_uleb(&mut out, 5);
                put_uleb(&mut out, text.len() as u64);
                put_uleb(&mut out, text.len() as u64);
                out.extend_from_slice(text.as_bytes());
            }
        }
        match id {
            2 => {
                put_uleb(&mut out, read_only.len() as u64);
                for &r in read_only {
                    put_uleb(&mut out, u64::from(r));
                }
            }
            16 => {
                put_uleb(&mut out, statics.len() as u64);
                for &r in statics {
                    put_uleb(&mut out, u64::from(r));
                }
            }
            _ => {
                let from = g.first[i] as usize;
                let to = g.first.get(i + 1).map_or(g.refs.len(), |&t| t as usize);
                put_uleb(&mut out, (to - from) as u64);
                for &r in &g.refs[from..to] {
                    put_uleb(&mut out, u64::from(r));
                }
            }
        }
    }
    put_uleb(&mut out, 0);
    for &h in &g.hash {
        put_uleb(&mut out, u64::from(h));
    }
    out
}

/// `copies` remapped copies of `full` under one root. The first copy keeps its ids; the other
/// copies' objects (all but their roots) follow it, and their root's refs join the one root, so
/// every copy hangs off it. The class table is shared. Header totals scale with `copies`.
pub fn replicate(full: &FullSnapshot, copies: u32) -> FullSnapshot {
    assert!(copies >= 1);
    let n = full.objects.len() as u32;
    let remap = |copy: u32, r: u32| -> u32 {
        if r <= 1 || copy == 0 {
            r
        } else {
            r + copy * (n - 1)
        }
    };
    let mut objects: Vec<ObjectRecord> =
        Vec::with_capacity(full.objects.len() + (copies as usize - 1) * (n as usize - 1));
    objects.extend(full.objects.iter().cloned());
    for copy in 1..copies {
        objects[0]
            .refs
            .extend(full.objects[0].refs.iter().map(|&r| remap(copy, r)));
        for o in &full.objects[1..] {
            let mut o = o.clone();
            for r in &mut o.refs {
                *r = remap(copy, *r);
            }
            objects.push(o);
        }
    }
    let mut externals: Vec<ExternalDef> = Vec::new();
    for copy in 0..copies {
        for e in &full.externals {
            if copy == 0 || e.object > 1 {
                externals.push(ExternalDef {
                    object: remap(copy, e.object),
                    ..e.clone()
                });
            }
        }
    }
    let hashes = full.hashes.as_ref().map(|h| {
        let mut out = h.clone();
        for _ in 1..copies {
            out.extend_from_slice(&h[1..]);
        }
        out
    });
    let k = u64::from(copies);
    FullSnapshot {
        header: HeapHeader {
            flags: full.header.flags,
            name: full.header.name.clone(),
            shallow_size: full.header.shallow_size * k,
            capacity: full.header.capacity * k,
            external_size: full.header.external_size * k,
        },
        classes: full.classes.clone(),
        objects,
        externals,
        hashes,
    }
}
