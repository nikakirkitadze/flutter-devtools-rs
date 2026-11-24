//! The binary heap-snapshot format: types, the streaming parser, the full decoder and the
//! single-object reader. All integers are LEB128; see the grammar in the M4 facts (H9, H10).
//!
//! Nothing here recurses and nothing trusts a count: every length is consumed byte by byte
//! from a bounded buffer, so hostile input ends in an `Err` carrying a byte offset.

use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};

/// `offsets[k]` is the file offset of object `k * OFFSET_STRIDE + 1`'s record.
pub const OFFSET_STRIDE: u32 = 64;

const BUF: usize = 256 * 1024;
const PROGRESS_EVERY: u64 = 1 << 20;
const MAGIC: &[u8; 8] = b"dartheap";
/// Upper bound on a reservation when the caller can't say how long the file is.
const UNKNOWN_LEN_RESERVE: u64 = 1 << 26;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct HeapHeader {
    pub flags: u64,
    pub name: String,
    pub shallow_size: u64,
    pub capacity: u64,
    pub external_size: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClassDef {
    pub flags: u64,
    pub name: String,
    pub library_name: String,
    pub library_uri: String,
    pub reserved: String,
    pub fields: Vec<FieldDef>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct FieldDef {
    pub flags: u64,
    pub index: u32,
    pub name: String,
    pub reserved: String,
}

/// What an object's data is. Bool is split so one byte answers both null and bool checks.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    None,
    Null,
    False,
    True,
    Int,
    Double,
    Latin1,
    Utf16,
    Length,
    Name,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HeapError {
    BadMagic,
    Truncated { at: u64 },
    BadTag { tag: u64, at: u64 },
    OutOfRange { what: &'static str, at: u64 },
    Overflow(&'static str),
    Io(String),
    Cancelled,
}

impl fmt::Display for HeapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HeapError::BadMagic => write!(f, "not a Dart heap snapshot"),
            HeapError::Truncated { at } => write!(f, "snapshot ends early at byte {at}"),
            HeapError::BadTag { tag, at } => write!(f, "unknown data tag {tag} at byte {at}"),
            HeapError::OutOfRange { what, at } => write!(f, "{what} out of range at byte {at}"),
            HeapError::Overflow(what) => write!(f, "{what} is too large"),
            HeapError::Io(e) => write!(f, "read failed: {e}"),
            HeapError::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for HeapError {}

/// The compact graph. Object 0 is a sentinel; real objects are `1..=object_count()`.
/// References keep their positions, including the zeros (unboxed fields).
#[derive(Debug)]
pub struct Graph {
    pub header: HeapHeader,
    /// `[0]` is a sentinel; class ids are `1..`.
    pub classes: Vec<ClassDef>,
    pub class_of: Vec<u32>,
    pub shallow: Vec<u32>,
    pub kind: Vec<Kind>,
    /// CSR: object `o`'s refs are `refs[first_ref[o]..first_ref[o + 1]]`. `object_count + 2` entries.
    pub first_ref: Vec<u32>,
    pub refs: Vec<u32>,
    /// 0 = none.
    pub identity_hash: Vec<u32>,
    pub offsets: Vec<u32>,
    pub mint_class: Option<u32>,
}

impl Graph {
    pub fn object_count(&self) -> usize {
        self.class_of.len().saturating_sub(1)
    }

    pub fn refs_of(&self, o: u32) -> &[u32] {
        let o = o as usize;
        match (self.first_ref.get(o), self.first_ref.get(o + 1)) {
            (Some(&a), Some(&b)) => &self.refs[a as usize..b as usize],
            _ => &[],
        }
    }

    fn class(&self, cid: u32) -> Option<&ClassDef> {
        if cid == 0 {
            None
        } else {
            self.classes.get(cid as usize)
        }
    }

    /// The position in an instance's ref list of the class's first field called `name`.
    pub fn field_index(&self, cid: u32, name: &str) -> Option<usize> {
        self.class(cid)?
            .fields
            .iter()
            .find(|f| f.name == name)
            .map(|f| f.index as usize)
    }

    /// The name of the first field at ref position `pos`; `None` for unnamed positions.
    pub fn field_name_at(&self, cid: u32, pos: usize) -> Option<&str> {
        self.class(cid)?
            .fields
            .iter()
            .find(|f| f.index as usize == pos)
            .map(|f| f.name.as_str())
    }

    pub fn find_class(&self, name: &str, uri: &str) -> Option<u32> {
        self.classes
            .iter()
            .enumerate()
            .skip(1)
            .find(|(_, c)| c.name == name && c.library_uri == uri)
            .map(|(i, _)| i as u32)
    }

    /// The isolate object (the `Isolate` class is the static field table), found among the
    /// root's refs.
    pub fn isolate_object(&self) -> Option<u32> {
        let isolate_class = 4u32;
        if self.classes.get(isolate_class as usize)?.name != "Isolate" {
            return None;
        }
        self.refs_of(1)
            .iter()
            .copied()
            .find(|&r| r != 0 && self.class_of.get(r as usize) == Some(&isolate_class))
    }

    /// The values of every static called `name` (names are bare and not unique), in field order.
    pub fn static_values(&self, name: &str) -> Vec<u32> {
        let Some(iso) = self.isolate_object() else {
            return Vec::new();
        };
        let Some(class) = self.class(4) else {
            return Vec::new();
        };
        let refs = self.refs_of(iso);
        class
            .fields
            .iter()
            .filter(|f| f.name == name)
            .filter_map(|f| refs.get(f.index as usize).copied())
            .filter(|&r| r != 0)
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    None,
    Null,
    Bool(bool),
    Int(i64),
    Double(f64),
    /// `len` is the full length; `bytes` holds at most 128 of them.
    Latin1 {
        len: u32,
        bytes: Vec<u8>,
    },
    Utf16 {
        len: u32,
        units: Vec<u16>,
    },
    Length(u64),
    Name(String),
}

impl Value {
    pub fn text(&self) -> Option<String> {
        match self {
            Value::Latin1 { bytes, .. } => Some(bytes.iter().map(|&b| b as char).collect()),
            Value::Utf16 { units, .. } => Some(String::from_utf16_lossy(units)),
            Value::Name(s) => Some(s.clone()),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectRecord {
    pub class: u32,
    pub shallow: u64,
    pub value: Value,
    pub refs: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExternalDef {
    pub object: u32,
    pub size: u64,
    pub name: String,
}

/// Everything in the file. `classes` has no sentinel: class id `c` is `classes[c - 1]`;
/// `objects[o - 1]` is object `o`. `hashes` is `None` when the file ends after the externals.
#[derive(Debug, Clone, PartialEq)]
pub struct FullSnapshot {
    pub header: HeapHeader,
    pub classes: Vec<ClassDef>,
    pub objects: Vec<ObjectRecord>,
    pub externals: Vec<ExternalDef>,
    pub hashes: Option<Vec<u32>>,
}

/// The id of class `_Mint` (whose integers are SLEB), given classes without a sentinel.
pub(crate) fn find_mint(classes: &[ClassDef]) -> Option<u32> {
    let mut any = None;
    for (i, c) in classes.iter().enumerate() {
        if c.name == "_Mint" {
            if c.library_uri == "dart:core" {
                return Some(i as u32 + 1);
            }
            any.get_or_insert(i as u32 + 1);
        }
    }
    any
}

// ---------------------------------------------------------------------------------------------
// Byte source

struct Src<'a, R: Read> {
    r: R,
    buf: Box<[u8]>,
    pos: usize,
    end: usize,
    /// File offset of `buf[0]`.
    base: u64,
    cancel: &'a AtomicBool,
    progress: &'a mut dyn FnMut(u64),
    last_progress: u64,
}

impl<'a, R: Read> Src<'a, R> {
    fn new(
        r: R,
        cap: usize,
        base: u64,
        cancel: &'a AtomicBool,
        progress: &'a mut dyn FnMut(u64),
    ) -> Self {
        Src {
            r,
            buf: vec![0; cap.max(16)].into_boxed_slice(),
            pos: 0,
            end: 0,
            base,
            cancel,
            progress,
            last_progress: base,
        }
    }

    fn offset(&self) -> u64 {
        self.base + self.pos as u64
    }

    /// Ensures a byte is buffered; `false` at end of input.
    fn fill(&mut self) -> Result<bool, HeapError> {
        if self.pos < self.end {
            return Ok(true);
        }
        if self.cancel.load(Ordering::Relaxed) {
            return Err(HeapError::Cancelled);
        }
        self.base += self.end as u64;
        self.pos = 0;
        self.end = 0;
        loop {
            match self.r.read(&mut self.buf) {
                Ok(0) => return Ok(false),
                Ok(n) => {
                    self.end = n;
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(HeapError::Io(e.to_string())),
            }
        }
        let read = self.base + self.end as u64;
        if read - self.last_progress >= PROGRESS_EVERY {
            self.last_progress = read;
            (self.progress)(read);
        }
        Ok(true)
    }

    /// Fails when bytes follow the last section: the file is more than one snapshot.
    fn end_of_input(&mut self) -> Result<(), HeapError> {
        if self.fill()? {
            return Err(HeapError::OutOfRange {
                what: "trailing bytes",
                at: self.offset(),
            });
        }
        Ok(())
    }

    fn byte(&mut self) -> Result<u8, HeapError> {
        if !self.fill()? {
            return Err(HeapError::Truncated { at: self.offset() });
        }
        let b = self.buf[self.pos];
        self.pos += 1;
        Ok(b)
    }

    fn uleb(&mut self) -> Result<u64, HeapError> {
        let mut v = 0u64;
        let mut shift = 0u32;
        loop {
            let b = self.byte()?;
            if shift == 63 && b > 1 {
                return Err(HeapError::Overflow("LEB128 integer"));
            }
            v |= u64::from(b & 0x7f) << shift;
            if b < 0x80 {
                return Ok(v);
            }
            shift += 7;
        }
    }

    fn sleb(&mut self) -> Result<i64, HeapError> {
        let mut v = 0u64;
        let mut shift = 0u32;
        loop {
            let b = self.byte()?;
            if shift == 63 && b >= 0x80 {
                return Err(HeapError::Overflow("LEB128 integer"));
            }
            v |= u64::from(b & 0x7f) << shift;
            shift += 7;
            if b < 0x80 {
                if shift < 64 && b & 0x40 != 0 {
                    v |= u64::MAX << shift;
                }
                return Ok(v as i64);
            }
        }
    }

    /// Skips one LEB128 integer of either signedness.
    fn skip_leb(&mut self) -> Result<(), HeapError> {
        for _ in 0..10 {
            if self.byte()? < 0x80 {
                return Ok(());
            }
        }
        Err(HeapError::Overflow("LEB128 integer"))
    }

    fn skip(&mut self, mut n: u64) -> Result<(), HeapError> {
        while n > 0 {
            if !self.fill()? {
                return Err(HeapError::Truncated { at: self.offset() });
            }
            let take = ((self.end - self.pos) as u64).min(n);
            self.pos += take as usize;
            n -= take;
        }
        Ok(())
    }

    /// Appends `n` bytes to `out`, growing only as bytes actually arrive.
    fn read_vec(&mut self, mut n: u64, out: &mut Vec<u8>) -> Result<(), HeapError> {
        while n > 0 {
            if !self.fill()? {
                return Err(HeapError::Truncated { at: self.offset() });
            }
            let take = ((self.end - self.pos) as u64).min(n) as usize;
            out.extend_from_slice(&self.buf[self.pos..self.pos + take]);
            self.pos += take;
            n -= take as u64;
        }
        Ok(())
    }

    fn read_str(&mut self) -> Result<String, HeapError> {
        let n = self.uleb()?;
        let mut v = Vec::new();
        self.read_vec(n, &mut v)?;
        Ok(String::from_utf8_lossy(&v).into_owned())
    }

    fn skip_str(&mut self) -> Result<(), HeapError> {
        let n = self.uleb()?;
        self.skip(n)
    }

    fn u32(&mut self, what: &'static str) -> Result<u32, HeapError> {
        u32::try_from(self.uleb()?).map_err(|_| HeapError::Overflow(what))
    }
}

fn reserve_len(count: u64, len_hint: u64) -> usize {
    count.min(if len_hint > 0 {
        len_hint
    } else {
        UNKNOWN_LEN_RESERVE
    }) as usize
}

// ---------------------------------------------------------------------------------------------
// Shared grammar

struct Prelude {
    header: HeapHeader,
    /// No sentinel.
    classes: Vec<ClassDef>,
    ref_count: u64,
    object_count: u64,
}

struct Limits {
    classes: u64,
    objects: u64,
}

fn read_prelude<R: Read>(s: &mut Src<R>, len_hint: u64) -> Result<Prelude, HeapError> {
    for &m in MAGIC {
        if s.byte()? != m {
            return Err(HeapError::BadMagic);
        }
    }
    let header = HeapHeader {
        flags: s.uleb()?,
        name: s.read_str()?,
        shallow_size: s.uleb()?,
        capacity: s.uleb()?,
        external_size: s.uleb()?,
    };
    let class_count = s.uleb()?;
    if class_count > u64::from(u32::MAX) {
        return Err(HeapError::Overflow("class count"));
    }
    if len_hint > 0 && class_count > len_hint {
        return Err(HeapError::OutOfRange {
            what: "class count",
            at: s.offset(),
        });
    }
    let mut classes = Vec::with_capacity(reserve_len(class_count, len_hint));
    for _ in 0..class_count {
        let flags = s.uleb()?;
        let name = s.read_str()?;
        let library_name = s.read_str()?;
        let library_uri = s.read_str()?;
        let reserved = s.read_str()?;
        let field_count = s.uleb()?;
        let mut fields = Vec::with_capacity(field_count.min(1024) as usize);
        for _ in 0..field_count {
            fields.push(FieldDef {
                flags: s.uleb()?,
                index: s.u32("field index")?,
                name: s.read_str()?,
                reserved: s.read_str()?,
            });
        }
        classes.push(ClassDef {
            flags,
            name,
            library_name,
            library_uri,
            reserved,
            fields,
        });
    }
    let ref_count = s.uleb()?;
    let at = s.offset();
    let object_count = s.uleb()?;
    if ref_count > u64::from(u32::MAX) {
        return Err(HeapError::Overflow("reference count"));
    }
    if object_count > u64::from(u32::MAX) - 2 {
        return Err(HeapError::Overflow("object count"));
    }
    if len_hint > 0 && (object_count > len_hint || ref_count > len_hint) {
        return Err(HeapError::OutOfRange {
            what: "count exceeds the file size",
            at,
        });
    }
    Ok(Prelude {
        header,
        classes,
        ref_count,
        object_count,
    })
}

fn read_class_id<R: Read>(s: &mut Src<R>, lim: &Limits) -> Result<u32, HeapError> {
    let at = s.offset();
    let c = s.u32("class id")?;
    if c == 0 || u64::from(c) > lim.classes {
        return Err(HeapError::OutOfRange {
            what: "class id",
            at,
        });
    }
    Ok(c)
}

fn read_ref<R: Read>(s: &mut Src<R>, lim: &Limits) -> Result<u32, HeapError> {
    let at = s.offset();
    let r = s.u32("object ref")?;
    if u64::from(r) > lim.objects {
        return Err(HeapError::OutOfRange {
            what: "object ref",
            at,
        });
    }
    Ok(r)
}

/// Reads one object, appending its refs to `refs` and skipping its data.
fn scan_object<R: Read>(
    s: &mut Src<R>,
    lim: &Limits,
    refs: &mut Vec<u32>,
) -> Result<(u32, u64, Kind), HeapError> {
    let class = read_class_id(s, lim)?;
    let shallow = s.uleb()?;
    let tag_at = s.offset();
    let tag = s.uleb()?;
    let kind = match tag {
        0 => Kind::None,
        1 => Kind::Null,
        2 => {
            if s.uleb()? != 0 {
                Kind::True
            } else {
                Kind::False
            }
        }
        3 => {
            s.skip_leb()?;
            Kind::Int
        }
        4 => {
            s.skip(8)?;
            Kind::Double
        }
        5 => {
            s.skip_leb()?;
            let trunc = s.uleb()?;
            s.skip(trunc)?;
            Kind::Latin1
        }
        6 => {
            s.skip_leb()?;
            let trunc = s.uleb()?;
            s.skip(
                trunc
                    .checked_mul(2)
                    .ok_or(HeapError::Overflow("string length"))?,
            )?;
            Kind::Utf16
        }
        7 => {
            s.skip_leb()?;
            Kind::Length
        }
        8 => {
            s.skip_str()?;
            Kind::Name
        }
        tag => return Err(HeapError::BadTag { tag, at: tag_at }),
    };
    let n = s.uleb()?;
    for _ in 0..n {
        refs.push(read_ref(s, lim)?);
    }
    Ok((class, shallow, kind))
}

/// Reads one object fully.
fn read_record<R: Read>(
    s: &mut Src<R>,
    lim: &Limits,
    mint_class: Option<u32>,
) -> Result<ObjectRecord, HeapError> {
    let class = read_class_id(s, lim)?;
    let shallow = s.uleb()?;
    let tag_at = s.offset();
    let tag = s.uleb()?;
    let value = match tag {
        0 => Value::None,
        1 => Value::Null,
        2 => Value::Bool(s.uleb()? != 0),
        3 => {
            if Some(class) == mint_class {
                Value::Int(s.sleb()?)
            } else {
                Value::Int(s.uleb()? as i64)
            }
        }
        4 => {
            let mut b = Vec::with_capacity(8);
            s.read_vec(8, &mut b)?;
            Value::Double(f64::from_le_bytes(b.try_into().expect("8 bytes")))
        }
        5 => {
            let len = s.u32("string length")?;
            let trunc = s.uleb()?;
            let mut bytes = Vec::new();
            s.read_vec(trunc, &mut bytes)?;
            Value::Latin1 { len, bytes }
        }
        6 => {
            let len = s.u32("string length")?;
            let trunc = s.uleb()?;
            let mut raw = Vec::new();
            s.read_vec(
                trunc
                    .checked_mul(2)
                    .ok_or(HeapError::Overflow("string length"))?,
                &mut raw,
            )?;
            let units = raw
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_le_bytes(*c))
                .collect();
            Value::Utf16 { len, units }
        }
        7 => Value::Length(s.uleb()?),
        8 => Value::Name(s.read_str()?),
        tag => return Err(HeapError::BadTag { tag, at: tag_at }),
    };
    let n = s.uleb()?;
    let mut refs = Vec::with_capacity(n.min(4096) as usize);
    for _ in 0..n {
        refs.push(read_ref(s, lim)?);
    }
    Ok(ObjectRecord {
        class,
        shallow,
        value,
        refs,
    })
}

// ---------------------------------------------------------------------------------------------
// Entry points

/// Streams a snapshot into the compact graph through a 256 KiB buffer. Vectors are reserved
/// from the header's counts (`len_hint` = the file length, 0 if unknown); string bytes are skipped.
pub fn parse<R: Read>(
    r: R,
    len_hint: u64,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(u64),
) -> Result<Graph, HeapError> {
    let mut s = Src::new(r, BUF, 0, cancel, progress);
    let Prelude {
        header,
        mut classes,
        ref_count,
        object_count,
    } = read_prelude(&mut s, len_hint)?;
    let mint_class = find_mint(&classes);
    let lim = Limits {
        classes: classes.len() as u64,
        objects: object_count,
    };
    classes.insert(0, ClassDef::default());

    let n = object_count as usize;
    let mut class_of = Vec::with_capacity(n + 1);
    let mut shallow = Vec::with_capacity(n + 1);
    let mut kind = Vec::with_capacity(n + 1);
    let mut first_ref = Vec::with_capacity(n + 2);
    let mut refs: Vec<u32> = Vec::with_capacity(reserve_len(ref_count, len_hint));
    let mut offsets = Vec::with_capacity(n.div_ceil(OFFSET_STRIDE as usize));
    class_of.push(0);
    shallow.push(0);
    kind.push(Kind::None);
    first_ref.push(0);

    for o in 1..=object_count {
        if (o - 1) % u64::from(OFFSET_STRIDE) == 0 {
            let off = u32::try_from(s.offset()).map_err(|_| HeapError::Overflow("file offset"))?;
            offsets.push(off);
        }
        first_ref.push(refs.len() as u32);
        let (c, sh, k) = scan_object(&mut s, &lim, &mut refs)?;
        if refs.len() > u32::MAX as usize {
            return Err(HeapError::Overflow("reference count"));
        }
        class_of.push(c);
        shallow.push(u32::try_from(sh).map_err(|_| HeapError::Overflow("shallow size"))?);
        kind.push(k);
    }
    first_ref.push(refs.len() as u32);

    let external_count = s.uleb()?;
    for _ in 0..external_count {
        let at = s.offset();
        let o = s.uleb()?;
        if o == 0 || o > object_count {
            return Err(HeapError::OutOfRange {
                what: "external object",
                at,
            });
        }
        s.skip_leb()?;
        s.skip_str()?;
    }

    let mut identity_hash = Vec::with_capacity(n + 1);
    identity_hash.push(0);
    if s.fill()? {
        for _ in 0..n {
            identity_hash.push(s.u32("identity hash")?);
        }
        s.end_of_input()?;
    } else {
        identity_hash.resize(n + 1, 0);
    }

    Ok(Graph {
        header,
        classes,
        class_of,
        shallow,
        kind,
        first_ref,
        refs,
        identity_hash,
        offsets,
        mint_class,
    })
}

/// Decodes every byte of a snapshot. Meant for tests, fixtures and tools, not the big path.
pub fn decode_full(bytes: &[u8]) -> Result<FullSnapshot, HeapError> {
    let cancel = AtomicBool::new(false);
    let mut noop = |_: u64| {};
    let len = bytes.len() as u64;
    let mut s = Src::new(bytes, bytes.len().clamp(16, BUF), 0, &cancel, &mut noop);
    let Prelude {
        header,
        classes,
        ref_count: _,
        object_count,
    } = read_prelude(&mut s, len)?;
    let mint = find_mint(&classes);
    let lim = Limits {
        classes: classes.len() as u64,
        objects: object_count,
    };

    let mut objects = Vec::with_capacity(reserve_len(object_count, len));
    for _ in 0..object_count {
        objects.push(read_record(&mut s, &lim, mint)?);
    }

    let external_count = s.uleb()?;
    let mut externals = Vec::with_capacity(external_count.min(4096) as usize);
    for _ in 0..external_count {
        let at = s.offset();
        let object = s.u32("external object")?;
        if object == 0 || u64::from(object) > object_count {
            return Err(HeapError::OutOfRange {
                what: "external object",
                at,
            });
        }
        externals.push(ExternalDef {
            object,
            size: s.uleb()?,
            name: s.read_str()?,
        });
    }

    let hashes = if s.fill()? {
        let mut h = Vec::with_capacity(objects.len());
        for _ in 0..object_count {
            h.push(s.u32("identity hash")?);
        }
        s.end_of_input()?;
        Some(h)
    } else {
        None
    };
    Ok(FullSnapshot {
        header,
        classes,
        objects,
        externals,
        hashes,
    })
}

fn to_io(e: HeapError) -> io::Error {
    match e {
        HeapError::Io(m) => io::Error::other(m),
        e => io::Error::new(io::ErrorKind::InvalidData, e),
    }
}

/// Reads one object: seeks to the stride mark at or before it and decodes forward.
pub fn read_object(
    file: &File,
    offsets: &[u32],
    mint_class: Option<u32>,
    object: u32,
) -> io::Result<ObjectRecord> {
    let bad = || io::Error::new(io::ErrorKind::InvalidInput, "no such object");
    if object == 0 {
        return Err(bad());
    }
    let off = *offsets
        .get(((object - 1) / OFFSET_STRIDE) as usize)
        .ok_or_else(bad)?;
    let mut f = file;
    f.seek(SeekFrom::Start(u64::from(off)))?;
    let cancel = AtomicBool::new(false);
    let mut noop = |_: u64| {};
    let mut s = Src::new(f, 16 * 1024, u64::from(off), &cancel, &mut noop);
    let lim = Limits {
        classes: u64::from(u32::MAX),
        objects: u64::from(u32::MAX),
    };
    let mut scratch = Vec::new();
    for _ in 0..(object - 1) % OFFSET_STRIDE {
        scratch.clear();
        scan_object(&mut s, &lim, &mut scratch).map_err(to_io)?;
    }
    read_record(&mut s, &lim, mint_class).map_err(to_io)
}

#[cfg(test)]
mod tests {
    #[test]
    fn bytes_after_the_last_section_are_an_error() {
        let mut b = SnapshotBuilder::new();
        let o = b.object(1, 8, Value::None, &[]);
        b.hash(o, 5);
        let one = encode_full(&b.finish());
        let cancel = AtomicBool::new(false);
        let at = one.len() as u64;
        let trailing = |e: HeapError| {
            e == HeapError::OutOfRange {
                what: "trailing bytes",
                at,
            }
        };
        // The snapshot alone is fine; one more byte, or a whole second snapshot, is not.
        parse(&one[..], at, &cancel, &mut |_| {}).unwrap();
        decode_full(&one).unwrap();
        let mut extra = one.clone();
        extra.push(0);
        assert!(trailing(
            parse(&extra[..], at, &cancel, &mut |_| {}).unwrap_err()
        ));
        assert!(trailing(decode_full(&extra).unwrap_err()));
        let mut two = one.clone();
        two.extend_from_slice(&one);
        assert!(trailing(
            parse(&two[..], at, &cancel, &mut |_| {}).unwrap_err()
        ));
        assert!(trailing(decode_full(&two).unwrap_err()));
    }

    use super::*;
    use crate::heap::encode::{SnapshotBuilder, encode_full, put_str, put_uleb};
    use std::io::Write;

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
    }

    fn run_parse(bytes: &[u8]) -> Result<Graph, HeapError> {
        parse(
            bytes,
            bytes.len() as u64,
            &AtomicBool::new(false),
            &mut |_| {},
        )
    }

    fn kind_of(v: &Value) -> Kind {
        match v {
            Value::None => Kind::None,
            Value::Null => Kind::Null,
            Value::Bool(false) => Kind::False,
            Value::Bool(true) => Kind::True,
            Value::Int(_) => Kind::Int,
            Value::Double(_) => Kind::Double,
            Value::Latin1 { .. } => Kind::Latin1,
            Value::Utf16 { .. } => Kind::Utf16,
            Value::Length(_) => Kind::Length,
            Value::Name(_) => Kind::Name,
        }
    }

    fn random_value(r: &mut Rng) -> Value {
        match r.below(10) {
            0 => Value::None,
            1 => Value::Null,
            2 => Value::Bool(r.below(2) == 0),
            3 => Value::Int(r.next() as i64 >> r.below(64)),
            4 => Value::Double(r.below(1000) as f64 / 8.0 - 20.0),
            5 => Value::Latin1 {
                len: 0,
                bytes: (0..r.below(200)).map(|_| r.below(256) as u8).collect(),
            },
            6 => Value::Utf16 {
                len: 0,
                units: (0..r.below(200)).map(|_| r.below(65536) as u16).collect(),
            },
            7 => Value::Length(r.next() >> r.below(64)),
            8 => Value::Name(format!("name{}", r.below(100))),
            _ => Value::Int(-(r.below(5) as i64)),
        }
    }

    fn fix_len(v: Value) -> Value {
        match v {
            Value::Latin1 { bytes, .. } => Value::Latin1 {
                len: bytes.len() as u32,
                bytes,
            },
            Value::Utf16 { units, .. } => Value::Utf16 {
                len: units.len() as u32,
                units,
            },
            v => v,
        }
    }

    fn random_snapshot(seed: u64) -> FullSnapshot {
        let mut r = Rng(seed);
        let mut b = SnapshotBuilder::new();
        let mint = b.class("_Mint", "dart:core", &[]);
        let mut classes = vec![mint, b.class("_Smi", "dart:core", &[])];
        for i in 0..r.below(4) {
            classes.push(b.class(&format!("C{i}"), "package:x/x.dart", &["a", "b", "a"]));
        }
        let n = r.below(40) as u32;
        let mut ids = Vec::new();
        for _ in 0..n {
            let c = classes[r.below(classes.len() as u64) as usize];
            let v = fix_len(random_value(&mut r));
            ids.push(b.object(c, r.below(5000), v, &[]));
        }
        let total = 5 + n;
        for &o in &ids {
            let refs: Vec<u32> = (0..r.below(6))
                .map(|_| r.below(u64::from(total) + 1) as u32)
                .collect();
            b.set_refs(o, &refs);
        }
        for _ in 0..r.below(3) {
            let target = r.below(u64::from(total)) as u32 + 1;
            b.static_field("s", target);
        }
        if r.below(3) != 0 {
            for o in 1..=total {
                if r.below(2) == 0 {
                    b.hash(o, r.next() as u32);
                }
            }
        }
        for _ in 0..r.below(3) {
            let o = r.below(u64::from(total)) as u32 + 1;
            b.external(o, r.below(100), "sym");
        }
        b.finish()
    }

    fn sample() -> FullSnapshot {
        let mut b = SnapshotBuilder::new();
        let mint = b.class("_Mint", "dart:core", &[]);
        let smi = b.class("_Smi", "dart:core", &[]);
        let any = b.class("Any", "dart:core", &["f"]);
        b.object(smi, 0, Value::Int(-1), &[]);
        b.object(mint, 0, Value::Int(i64::MIN + 5), &[]);
        b.object(mint, 0, Value::Int(64), &[]);
        b.object(any, 8, Value::None, &[0, 1]);
        b.object(any, 8, Value::Null, &[]);
        b.object(any, 8, Value::Bool(true), &[]);
        b.object(any, 8, Value::Bool(false), &[]);
        b.object(any, 8, Value::Double(-2.5), &[]);
        b.object(
            any,
            8,
            Value::Latin1 {
                len: 3,
                bytes: b"abc".to_vec(),
            },
            &[],
        );
        let long: Vec<u16> = (0..300).map(|i| 0x4e00 + i as u16).collect();
        b.object(
            any,
            8,
            Value::Utf16 {
                len: 300,
                units: long,
            },
            &[],
        );
        b.object(any, 8, Value::Length(1 << 40), &[]);
        b.object(any, 8, Value::Name("hello".into()), &[]);
        b.finish()
    }

    #[test]
    fn round_trip_every_data_tag() {
        let s = sample();
        let bytes = encode_full(&s);
        // A negative Smi is a 10-byte ULEB.
        let ten = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01];
        assert!(bytes.windows(10).any(|w| w == ten));
        let back = decode_full(&bytes).unwrap();
        assert_eq!(back, s);
        // UTF-16 past 128 units is cut, keeping the full length.
        let last_utf16 = back.objects.iter().find_map(|o| match &o.value {
            Value::Utf16 { len, units } => Some((*len, units.len())),
            _ => None,
        });
        assert_eq!(last_utf16, Some((300, 128)));
    }

    #[test]
    fn parse_agrees_with_decode_full() {
        for seed in 0..1000 {
            let s = random_snapshot(seed);
            let bytes = encode_full(&s);
            let full = decode_full(&bytes).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
            assert_eq!(full, s, "seed {seed}");
            let g = run_parse(&bytes).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
            assert_eq!(g.object_count(), s.objects.len());
            assert_eq!(g.classes.len(), s.classes.len() + 1);
            assert_eq!(&g.classes[1..], &s.classes[..]);
            assert_eq!(g.first_ref.len(), s.objects.len() + 2);
            for (i, o) in s.objects.iter().enumerate() {
                let id = i as u32 + 1;
                assert_eq!(g.class_of[i + 1], o.class, "seed {seed} obj {id}");
                assert_eq!(u64::from(g.shallow[i + 1]), o.shallow);
                assert_eq!(g.kind[i + 1], kind_of(&o.value), "seed {seed} obj {id}");
                assert_eq!(g.refs_of(id), &o.refs[..], "seed {seed} obj {id}");
                let h = s.hashes.as_ref().map_or(0, |h| h[i]);
                assert_eq!(g.identity_hash[i + 1], h);
            }
            assert_eq!(g.mint_class, find_mint(&s.classes));
            assert_eq!(g.refs_of(0), &[] as &[u32]);
        }
    }

    #[test]
    fn hashes_are_uleb() {
        let mut b = SnapshotBuilder::new();
        let o = b.object(1, 0, Value::None, &[]);
        let without = encode_full(&b.finish());
        let mut b = SnapshotBuilder::new();
        let o2 = b.object(1, 0, Value::None, &[]);
        assert_eq!(o, o2);
        b.hash(o, 3_000_000_000);
        let snap = b.finish();
        let bytes = encode_full(&snap);
        // `without` has no hashes at all; the hash array is 6 objects of one byte each plus
        // the 3e9 hash widening from 1 to 5 bytes.
        assert_eq!(bytes.len() - without.len(), 6 + 4);
        assert_eq!(&bytes[bytes.len() - 5..], &[0x80, 0xbc, 0xc1, 0x96, 0x0b]);
        let g = run_parse(&bytes).unwrap();
        assert_eq!(g.identity_hash[o as usize], 3_000_000_000);
        assert_eq!(decode_full(&bytes).unwrap(), snap);
    }

    #[test]
    fn name_tag_is_8() {
        let mut b = SnapshotBuilder::new();
        b.object(1, 0, Value::Name("ab".into()), &[]);
        let bytes = encode_full(&b.finish());
        assert!(bytes.windows(5).any(|w| w == [8, 2, b'a', b'b', 0]));
    }

    #[test]
    fn mint_is_sleb_and_smi_is_uleb() {
        let mut b = SnapshotBuilder::new();
        let mint = b.class("_Mint", "dart:core", &[]);
        let smi = b.class("_Smi", "dart:core", &[]);
        b.object(mint, 0, Value::Int(-1), &[]);
        b.object(mint, 0, Value::Int(64), &[]);
        b.object(smi, 0, Value::Int(64), &[]);
        let bytes = encode_full(&b.finish());
        let rec = |c: u8, data: &[u8]| {
            let mut v = vec![c, 0, 3];
            v.extend_from_slice(data);
            v.push(0);
            v
        };
        let has = |needle: Vec<u8>| bytes.windows(needle.len()).any(|w| w == &needle[..]);
        assert!(has(rec(6, &[0x7f])), "mint -1 is one SLEB byte");
        assert!(has(rec(6, &[0xc0, 0x00])), "mint 64 is two SLEB bytes");
        assert!(has(rec(7, &[0x40])), "smi 64 is one ULEB byte");
    }

    #[test]
    fn every_truncation_is_an_error() {
        let mut r = Rng(7);
        let mut b = SnapshotBuilder::new();
        let mint = b.class("_Mint", "dart:core", &[]);
        let c = b.class("Thing", "package:x/x.dart", &["a", "b"]);
        for i in 0..50u32 {
            let v = fix_len(random_value(&mut r));
            let class = if i % 5 == 0 { mint } else { c };
            b.object(class, 16, v, &[(i % 7) + 1, 0, 3]);
            b.hash(6 + i, 1_000_000 + i);
        }
        b.external(6, 10, "finalizer");
        let snap = b.finish();
        let full = encode_full(&snap);
        let mut no_hashes = snap.clone();
        no_hashes.hashes = None;
        let ext_end = encode_full(&no_hashes).len();
        for n in 0..full.len() {
            let prefix = &full[..n];
            let p = run_parse(prefix);
            let d = decode_full(prefix);
            if n == ext_end {
                assert!(p.is_ok() && d.is_ok(), "prefix {n} is the no-hash form");
            } else {
                assert!(p.is_err(), "parse accepted prefix {n} of {}", full.len());
                assert!(d.is_err(), "decode accepted prefix {n} of {}", full.len());
            }
        }
        assert!(run_parse(&full).is_ok());
    }

    /// A one-class snapshot (class 1, no fields) around raw object bytes.
    fn raw(objects: &[&[u8]]) -> Vec<u8> {
        let mut v = b"dartheap".to_vec();
        put_uleb(&mut v, 0);
        put_str(&mut v, "");
        for _ in 0..3 {
            put_uleb(&mut v, 0);
        }
        put_uleb(&mut v, 1);
        put_uleb(&mut v, 0);
        for _ in 0..4 {
            put_str(&mut v, "C");
        }
        put_uleb(&mut v, 0);
        put_uleb(&mut v, 0);
        put_uleb(&mut v, objects.len() as u64);
        for o in objects {
            v.extend_from_slice(o);
        }
        put_uleb(&mut v, 0);
        v
    }

    #[test]
    fn bad_magic_and_bad_tag() {
        let ok = raw(&[&[1, 0, 1, 0]]);
        assert!(run_parse(&ok).is_ok());
        let mut bad = ok.clone();
        bad[0] = b'D';
        assert_eq!(run_parse(&bad).unwrap_err(), HeapError::BadMagic);
        assert_eq!(decode_full(&bad).unwrap_err(), HeapError::BadMagic);
        assert_eq!(run_parse(b"hello").unwrap_err(), HeapError::BadMagic);

        let bytes = raw(&[&[1, 0, 9, 0]]);
        let at = bytes.iter().position(|&b| b == 9).unwrap() as u64;
        assert_eq!(
            run_parse(&bytes).unwrap_err(),
            HeapError::BadTag { tag: 9, at }
        );
        assert_eq!(
            decode_full(&bytes).unwrap_err(),
            HeapError::BadTag { tag: 9, at }
        );
    }

    #[test]
    fn zero_objects_parses() {
        let bytes = raw(&[]);
        let g = run_parse(&bytes).unwrap();
        assert_eq!(g.object_count(), 0);
        assert_eq!(g.classes.len(), 2);
        assert_eq!(g.first_ref.len(), 2);
        assert!(g.refs_of(0).is_empty() && g.refs_of(1).is_empty());
        assert_eq!(g.identity_hash, vec![0]);
        assert_eq!(g.isolate_object(), None);
        assert!(g.static_values("x").is_empty());
        let full = decode_full(&bytes).unwrap();
        assert!(full.objects.is_empty());
    }

    #[test]
    fn out_of_range_ids_are_errors() {
        let ids = |tweak: &dyn Fn(&mut FullSnapshot)| {
            let mut b = SnapshotBuilder::new();
            b.object(1, 0, Value::Null, &[1]);
            let mut s = b.finish();
            tweak(&mut s);
            encode_full(&s)
        };
        let last = |s: &mut FullSnapshot| s.objects.len() - 1;
        let bytes = ids(&|s| {
            let i = last(s);
            s.objects[i].class = 0;
        });
        assert!(matches!(
            run_parse(&bytes).unwrap_err(),
            HeapError::OutOfRange {
                what: "class id",
                ..
            }
        ));
        assert!(decode_full(&bytes).is_err());
        let bytes = ids(&|s| {
            let i = last(s);
            s.objects[i].class = 99;
        });
        assert!(matches!(
            run_parse(&bytes).unwrap_err(),
            HeapError::OutOfRange {
                what: "class id",
                ..
            }
        ));
        let bytes = ids(&|s| {
            let i = last(s);
            s.objects[i].refs = vec![9999];
        });
        assert!(matches!(
            run_parse(&bytes).unwrap_err(),
            HeapError::OutOfRange {
                what: "object ref",
                ..
            }
        ));
        assert!(decode_full(&bytes).is_err());
        let bytes = ids(&|s| {
            let i = last(s);
            s.objects[i].refs = vec![u32::MAX];
            s.objects[i].class = u32::MAX;
        });
        assert!(run_parse(&bytes).is_err());
        // An external naming a missing object.
        let bytes = ids(&|s| {
            s.externals.push(ExternalDef {
                object: 500,
                size: 1,
                name: "x".into(),
            });
        });
        assert!(run_parse(&bytes).is_err());
        assert!(decode_full(&bytes).is_err());
    }

    #[test]
    fn oversized_leb_is_overflow_not_panic() {
        let mut bytes = raw(&[&[1, 0, 1, 0]]);
        // Replace the header's flags byte with an 11-byte integer.
        let mut hostile = bytes[..8].to_vec();
        hostile.extend(std::iter::repeat_n(0xff, 11));
        hostile.push(0x01);
        bytes.splice(8..9, hostile[8..].iter().copied());
        assert!(matches!(
            run_parse(&bytes).unwrap_err(),
            HeapError::Overflow(_)
        ));
        // A count far past the file size is refused before anything is reserved.
        let mut v = raw(&[&[1, 0, 1, 0]]);
        let n = v.len();
        v[n - 6] = 0x7f; // objectCount byte
        assert!(run_parse(&v).is_err());
    }

    #[test]
    fn no_hashes_parses() {
        let mut b = SnapshotBuilder::new();
        b.object(1, 4, Value::Null, &[]);
        b.external(6, 1, "x");
        let s = b.finish();
        assert!(s.hashes.is_none());
        let bytes = encode_full(&s);
        let g = run_parse(&bytes).unwrap();
        assert_eq!(g.identity_hash, vec![0; 7]);
        assert_eq!(decode_full(&bytes).unwrap().hashes, None);
    }

    #[test]
    fn read_object_at_stride_edges() {
        let mut r = Rng(99);
        let mut b = SnapshotBuilder::new();
        let mint = b.class("_Mint", "dart:core", &[]);
        for _ in 0..300 {
            let v = fix_len(random_value(&mut r));
            b.object(mint, 8, v, &[1, 2, 0]);
        }
        let s = b.finish();
        let bytes = encode_full(&s);
        let mut f = tempfile::tempfile().unwrap();
        f.write_all(&bytes).unwrap();
        let g = run_parse(&bytes).unwrap();
        assert_eq!(g.offsets.len(), s.objects.len().div_ceil(64));
        let n = s.objects.len() as u32;
        for o in [1, 2, 63, 64, 65, 66, 128, 129, n - 1, n] {
            let rec = read_object(&f, &g.offsets, g.mint_class, o).unwrap();
            assert_eq!(rec, s.objects[o as usize - 1], "object {o}");
        }
        assert!(read_object(&f, &g.offsets, g.mint_class, 0).is_err());
        assert!(read_object(&f, &g.offsets, g.mint_class, n + 1000).is_err());
    }

    /// A snapshot of about 2.5 MB: one object with 2,500,000 refs.
    fn big() -> Vec<u8> {
        let mut b = SnapshotBuilder::new();
        let o = b.object(1, 8, Value::Null, &[]);
        let refs: Vec<u32> = (0..2_500_000u32).map(|i| 1 + i % 6).collect();
        b.set_refs(o, &refs);
        encode_full(&b.finish())
    }

    struct Counting<'a> {
        inner: &'a [u8],
        max_request: usize,
    }
    impl Read for Counting<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.max_request = self.max_request.max(buf.len());
            self.inner.read(buf)
        }
    }

    #[test]
    fn parse_streams() {
        let bytes = big();
        assert!(bytes.len() > 2 * 1024 * 1024);
        let mut src = Counting {
            inner: &bytes,
            max_request: 0,
        };
        let mut seen = Vec::new();
        let g = parse(
            &mut src,
            bytes.len() as u64,
            &AtomicBool::new(false),
            &mut |n| seen.push(n),
        )
        .unwrap();
        assert!(
            src.max_request <= 256 * 1024,
            "asked for {}",
            src.max_request
        );
        assert_eq!(g.refs_of(6).len(), 2_500_000);
        assert!(seen.len() >= 2, "{seen:?}");
        assert!(
            seen.windows(2).all(|w| w[1] >= w[0] + 1024 * 1024),
            "{seen:?}"
        );
        assert!(seen.iter().all(|&n| n <= bytes.len() as u64));
    }

    #[test]
    fn cancel_stops_parse() {
        let bytes = big();
        let cancel = AtomicBool::new(true);
        assert_eq!(
            parse(&bytes[..], bytes.len() as u64, &cancel, &mut |_| {}).unwrap_err(),
            HeapError::Cancelled
        );
        // Cancelled from the progress callback, mid-stream.
        let cancel = AtomicBool::new(false);
        let err = parse(&bytes[..], bytes.len() as u64, &cancel, &mut |_| {
            cancel.store(true, Ordering::Relaxed);
        })
        .unwrap_err();
        assert_eq!(err, HeapError::Cancelled);
    }

    #[test]
    fn graph_lookups() {
        let mut b = SnapshotBuilder::new();
        let c = b.class(
            "AppEvents",
            "package:x/a.dart",
            &["_listeners", "x", "_listeners"],
        );
        let o = b.object(c, 8, Value::None, &[]);
        let p = b.object(c, 8, Value::None, &[]);
        b.static_field("instance", o);
        b.static_field("instance", p);
        let g = run_parse(&encode_full(&b.finish())).unwrap();
        assert_eq!(g.find_class("AppEvents", "package:x/a.dart"), Some(c));
        assert_eq!(g.find_class("AppEvents", "other"), None);
        assert_eq!(g.field_index(c, "_listeners"), Some(0));
        assert_eq!(g.field_index(c, "x"), Some(1));
        assert_eq!(g.field_name_at(c, 2), Some("_listeners"));
        assert_eq!(g.field_name_at(c, 9), None);
        assert_eq!(g.isolate_object(), Some(5));
        assert_eq!(g.static_values("instance"), vec![o, p]);
        assert!(g.static_values("nope").is_empty());
    }
}
