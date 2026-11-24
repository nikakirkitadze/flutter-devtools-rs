//! The snapshot encoder and a builder for fixtures. It writes integers exactly as the VM does:
//! SLEB for `_Mint`, ULEB for everything else (a negative Smi is a 10-byte ULEB).

use super::format::{
    ClassDef, ExternalDef, FieldDef, FullSnapshot, HeapHeader, ObjectRecord, Value, find_mint,
};

/// Strings are cut at this many code units, as the VM does.
const MAX_STRING_UNITS: usize = 128;

pub(crate) fn put_uleb(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

pub(crate) fn put_sleb(out: &mut Vec<u8>, mut v: i64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if (v == 0 && b & 0x40 == 0) || (v == -1 && b & 0x40 != 0) {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

pub(crate) fn put_str(out: &mut Vec<u8>, s: &str) {
    put_uleb(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

pub fn encode_full(s: &FullSnapshot) -> Vec<u8> {
    let mint = find_mint(&s.classes);
    let mut out = b"dartheap".to_vec();
    let h = &s.header;
    put_uleb(&mut out, h.flags);
    put_str(&mut out, &h.name);
    put_uleb(&mut out, h.shallow_size);
    put_uleb(&mut out, h.capacity);
    put_uleb(&mut out, h.external_size);

    put_uleb(&mut out, s.classes.len() as u64);
    for c in &s.classes {
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
        s.objects.iter().map(|o| o.refs.len() as u64).sum(),
    );
    put_uleb(&mut out, s.objects.len() as u64);
    for o in &s.objects {
        put_uleb(&mut out, u64::from(o.class));
        put_uleb(&mut out, o.shallow);
        match &o.value {
            Value::None => put_uleb(&mut out, 0),
            Value::Null => put_uleb(&mut out, 1),
            Value::Bool(b) => {
                put_uleb(&mut out, 2);
                put_uleb(&mut out, u64::from(*b));
            }
            Value::Int(v) => {
                put_uleb(&mut out, 3);
                if Some(o.class) == mint {
                    put_sleb(&mut out, *v);
                } else {
                    put_uleb(&mut out, *v as u64);
                }
            }
            Value::Double(d) => {
                put_uleb(&mut out, 4);
                out.extend_from_slice(&d.to_le_bytes());
            }
            Value::Latin1 { len, bytes } => {
                put_uleb(&mut out, 5);
                put_uleb(&mut out, u64::from(*len));
                put_uleb(&mut out, bytes.len() as u64);
                out.extend_from_slice(bytes);
            }
            Value::Utf16 { len, units } => {
                put_uleb(&mut out, 6);
                put_uleb(&mut out, u64::from(*len));
                put_uleb(&mut out, units.len() as u64);
                for u in units {
                    out.extend_from_slice(&u.to_le_bytes());
                }
            }
            Value::Length(n) => {
                put_uleb(&mut out, 7);
                put_uleb(&mut out, *n);
            }
            Value::Name(n) => {
                put_uleb(&mut out, 8);
                put_str(&mut out, n);
            }
        }
        put_uleb(&mut out, o.refs.len() as u64);
        for &r in &o.refs {
            put_uleb(&mut out, u64::from(r));
        }
    }

    put_uleb(&mut out, s.externals.len() as u64);
    for e in &s.externals {
        put_uleb(&mut out, u64::from(e.object));
        put_uleb(&mut out, e.size);
        put_str(&mut out, &e.name);
    }

    if let Some(hashes) = &s.hashes {
        for i in 0..s.objects.len() {
            put_uleb(&mut out, u64::from(hashes.get(i).copied().unwrap_or(0)));
        }
    }
    out
}

/// Builds a `FullSnapshot` in the fixed layout: classes 1-5 (`Root`, `Read-Only Pages`,
/// `Root slice`, `Isolate`, `ObjectStore`), object 1 the root, 2 the read-only object, 3 and 4
/// root slices, 5 the isolate object. The root's refs are `[2, 3, 4, 5]`.
pub struct SnapshotBuilder {
    classes: Vec<ClassDef>,
    objects: Vec<ObjectRecord>,
    externals: Vec<ExternalDef>,
    hashes: Vec<u32>,
    any_hash: bool,
}

impl Default for SnapshotBuilder {
    fn default() -> Self {
        Self::new()
    }
}

fn class_def(name: &str, uri: &str, fields: &[&str]) -> ClassDef {
    ClassDef {
        flags: 0,
        name: name.into(),
        library_name: uri.into(),
        library_uri: uri.into(),
        reserved: String::new(),
        fields: fields
            .iter()
            .enumerate()
            .map(|(i, f)| FieldDef {
                flags: 1,
                index: i as u32,
                name: (*f).into(),
                reserved: String::new(),
            })
            .collect(),
    }
}

fn clip(v: Value) -> Value {
    match v {
        Value::Latin1 { len, mut bytes } if bytes.len() > MAX_STRING_UNITS => {
            let len = len.max(bytes.len() as u32);
            bytes.truncate(MAX_STRING_UNITS);
            Value::Latin1 { len, bytes }
        }
        Value::Utf16 { len, mut units } if units.len() > MAX_STRING_UNITS => {
            let len = len.max(units.len() as u32);
            units.truncate(MAX_STRING_UNITS);
            Value::Utf16 { len, units }
        }
        v => v,
    }
}

impl SnapshotBuilder {
    pub fn new() -> Self {
        let mut b = SnapshotBuilder {
            classes: [
                "Root",
                "Read-Only Pages",
                "Root slice",
                "Isolate",
                "ObjectStore",
            ]
            .iter()
            .map(|n| class_def(n, "", &[]))
            .collect(),
            objects: Vec::new(),
            externals: Vec::new(),
            hashes: Vec::new(),
            any_hash: false,
        };
        let root = b.object(1, 0, Value::None, &[]);
        let ro = b.object(2, 0, Value::None, &[]);
        let s1 = b.object(3, 0, Value::Name("class table".into()), &[]);
        let s2 = b.object(3, 0, Value::Name("api state".into()), &[]);
        let iso = b.object(4, 0, Value::None, &[]);
        b.set_refs(root, &[ro, s1, s2, iso]);
        b
    }

    pub fn class(&mut self, name: &str, lib_uri: &str, fields: &[&str]) -> u32 {
        self.classes.push(class_def(name, lib_uri, fields));
        self.classes.len() as u32
    }

    pub fn object(&mut self, class: u32, shallow: u64, value: Value, refs: &[u32]) -> u32 {
        self.objects.push(ObjectRecord {
            class,
            shallow,
            value: clip(value),
            refs: refs.to_vec(),
        });
        self.objects.len() as u32
    }

    /// Sets an object's shallow size.
    pub fn set_shallow(&mut self, o: u32, shallow: u64) {
        self.objects[o as usize - 1].shallow = shallow;
    }

    pub fn set_refs(&mut self, o: u32, refs: &[u32]) {
        self.objects[o as usize - 1].refs = refs.to_vec();
    }

    /// Adds a static (an `Isolate` field) and the isolate object's matching ref. Returns the
    /// field's index.
    pub fn static_field(&mut self, name: &str, value_object: u32) -> u32 {
        let isolate = &mut self.classes[3];
        let index = isolate.fields.len() as u32;
        isolate.fields.push(FieldDef {
            flags: 1,
            index,
            name: name.into(),
            reserved: String::new(),
        });
        self.objects[4].refs.push(value_object);
        index
    }

    /// Makes the read-only object (2) reference `o`.
    pub fn read_only(&mut self, o: u32) {
        self.objects[1].refs.push(o);
    }

    pub fn hash(&mut self, o: u32, hash: u32) {
        let i = o as usize - 1;
        if self.hashes.len() <= i {
            self.hashes.resize(i + 1, 0);
        }
        self.hashes[i] = hash;
        self.any_hash = true;
    }

    pub fn external(&mut self, o: u32, size: u64, name: &str) {
        self.externals.push(ExternalDef {
            object: o,
            size,
            name: name.into(),
        });
    }

    pub fn finish(self) -> FullSnapshot {
        let shallow: u64 = self.objects.iter().map(|o| o.shallow).sum();
        let external: u64 = self.externals.iter().map(|e| e.size).sum();
        let mut hashes = self.hashes;
        hashes.resize(self.objects.len(), 0);
        FullSnapshot {
            header: HeapHeader {
                flags: 0,
                name: String::new(),
                shallow_size: shallow,
                capacity: shallow,
                external_size: external,
            },
            classes: self.classes,
            objects: self.objects,
            externals: self.externals,
            hashes: self.any_hash.then_some(hashes),
        }
    }
}
