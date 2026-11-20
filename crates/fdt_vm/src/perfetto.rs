//! Perfetto `getPerfettoCpuSamples` decoding. A small hand-written protobuf reader, limited to the
//! fields the Dart VM writes for CPU samples. Field numbers are in the spike's protocol facts.

use std::collections::HashMap;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde_json::json;

use crate::rpc::RpcError;
use crate::service::VmService;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PerfFunction {
    pub name: String,
    pub library: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PerfSampleRec {
    pub ts_ns: u64,
    pub tid: u32,
    /// Index into [`CpuProfile::stacks`].
    pub stack: u32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CpuProfile {
    pub functions: Vec<PerfFunction>,
    /// Root first, leaf last; indices into `functions`.
    pub stacks: Vec<Vec<u32>>,
    /// In wire order (not time-sorted).
    pub samples: Vec<PerfSampleRec>,
    /// Samples left out because their callstack, frame or function-name iid was not defined.
    pub skipped_samples: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuWindow {
    pub origin_us: i64,
    pub extent_us: i64,
    pub period_us: i64,
}

// ---- wire walker ----

#[derive(Clone, Copy)]
enum Wire<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

fn read_varint(buf: &[u8], pos: &mut usize) -> Result<u64, String> {
    let mut v: u64 = 0;
    let mut shift = 0;
    loop {
        let b = *buf.get(*pos).ok_or("truncated varint")?;
        *pos += 1;
        v |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Ok(v);
        }
        shift += 7;
        if shift > 63 {
            return Err("varint too long".into());
        }
    }
}

fn take<'a>(buf: &'a [u8], pos: &mut usize, n: usize) -> Result<&'a [u8], String> {
    let end = pos.checked_add(n).ok_or("length overflow")?;
    let b = buf.get(*pos..end).ok_or("truncated field")?;
    *pos = end;
    Ok(b)
}

/// Every `(field number, value)` in `buf`. Fixed-width values are skipped; they are never used.
fn fields(buf: &[u8]) -> Result<Vec<(u32, Wire<'_>)>, String> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < buf.len() {
        let tag = read_varint(buf, &mut pos)?;
        let field = u32::try_from(tag >> 3).map_err(|_| "field number too large")?;
        match tag & 7 {
            0 => out.push((field, Wire::Varint(read_varint(buf, &mut pos)?))),
            1 => {
                take(buf, &mut pos, 8)?;
            }
            2 => {
                let len =
                    usize::try_from(read_varint(buf, &mut pos)?).map_err(|_| "length too large")?;
                out.push((field, Wire::Bytes(take(buf, &mut pos, len)?)));
            }
            5 => {
                take(buf, &mut pos, 4)?;
            }
            t => return Err(format!("unsupported wire type {t}")),
        }
    }
    Ok(out)
}

fn num(w: Wire) -> u64 {
    match w {
        Wire::Varint(v) => v,
        Wire::Bytes(_) => 0,
    }
}

fn string(w: Wire) -> String {
    match w {
        Wire::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        Wire::Varint(_) => String::new(),
    }
}

/// A `repeated uint64` field, packed or not.
fn push_ids(out: &mut Vec<u64>, w: Wire) -> Result<(), String> {
    match w {
        Wire::Bytes(b) => {
            let mut p = 0;
            while p < b.len() {
                out.push(read_varint(b, &mut p)?);
            }
        }
        Wire::Varint(v) => out.push(v),
    }
    Ok(())
}

// ---- decoding ----

/// Interning tables, valid until the sequence clears them.
#[derive(Default)]
struct Interned {
    function_names: HashMap<u64, String>,
    /// frame iid -> (function name iid, mapping iid)
    frames: HashMap<u64, (u64, u64)>,
    callstacks: HashMap<u64, Vec<u64>>,
    mapping_paths: HashMap<u64, String>,
    /// mapping iid -> first path string iid
    mappings: HashMap<u64, u64>,
    /// frame iid -> index into the profile's functions
    resolved_frames: HashMap<u64, u32>,
    /// callstack iid -> index into the profile's stacks
    resolved_stacks: HashMap<u64, u32>,
}

#[derive(Default)]
struct Builder {
    profile: CpuProfile,
    function_index: HashMap<PerfFunction, u32>,
    stack_index: HashMap<Vec<u32>, u32>,
}

impl Interned {
    fn absorb(&mut self, data: &[u8]) -> Result<(), String> {
        for (n, w) in fields(data)? {
            let Wire::Bytes(b) = w else { continue };
            match n {
                5 | 17 => {
                    let (mut iid, mut s) = (0, String::new());
                    for (n, w) in fields(b)? {
                        match n {
                            1 => iid = num(w),
                            2 => s = string(w),
                            _ => {}
                        }
                    }
                    if n == 5 {
                        self.function_names.insert(iid, s);
                    } else {
                        self.mapping_paths.insert(iid, s);
                    }
                }
                6 => {
                    let (mut iid, mut name, mut mapping) = (0, 0, 0);
                    for (n, w) in fields(b)? {
                        match n {
                            1 => iid = num(w),
                            2 => name = num(w),
                            3 => mapping = num(w),
                            _ => {}
                        }
                    }
                    self.frames.insert(iid, (name, mapping));
                }
                7 => {
                    let (mut iid, mut ids) = (0, Vec::new());
                    for (n, w) in fields(b)? {
                        match n {
                            1 => iid = num(w),
                            2 => push_ids(&mut ids, w)?,
                            _ => {}
                        }
                    }
                    self.callstacks.insert(iid, ids);
                }
                19 => {
                    let (mut iid, mut first) = (0, None);
                    for (n, w) in fields(b)? {
                        match n {
                            1 => iid = num(w),
                            7 if first.is_none() => {
                                let mut ids = Vec::new();
                                push_ids(&mut ids, w)?;
                                first = ids.first().copied();
                            }
                            _ => {}
                        }
                    }
                    if let Some(p) = first {
                        self.mappings.insert(iid, p);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn function_of(&mut self, frame: u64, out: &mut Builder) -> Result<u32, String> {
        if let Some(&i) = self.resolved_frames.get(&frame) {
            return Ok(i);
        }
        let &(name_iid, mapping) = self
            .frames
            .get(&frame)
            .ok_or_else(|| format!("unknown frame {frame}"))?;
        let name = self
            .function_names
            .get(&name_iid)
            .ok_or_else(|| format!("unknown function name {name_iid}"))?;
        let library = (mapping != 0)
            .then(|| {
                let path = self.mappings.get(&mapping)?;
                self.mapping_paths.get(path).cloned()
            })
            .flatten();
        let f = PerfFunction {
            name: name.clone(),
            library,
        };
        let idx = match out.function_index.get(&f) {
            Some(&i) => i,
            None => {
                let i = out.profile.functions.len() as u32;
                out.profile.functions.push(f.clone());
                out.function_index.insert(f, i);
                i
            }
        };
        self.resolved_frames.insert(frame, idx);
        Ok(idx)
    }

    fn stack_of(&mut self, iid: u64, out: &mut Builder) -> Result<u32, String> {
        if let Some(&i) = self.resolved_stacks.get(&iid) {
            return Ok(i);
        }
        let frames = self
            .callstacks
            .get(&iid)
            .ok_or_else(|| format!("unknown callstack {iid}"))?
            .clone();
        let stack = frames
            .iter()
            .map(|&f| self.function_of(f, out))
            .collect::<Result<Vec<u32>, String>>()?;
        let idx = match out.stack_index.get(&stack) {
            Some(&i) => i,
            None => {
                let i = out.profile.stacks.len() as u32;
                out.profile.stacks.push(stack.clone());
                out.stack_index.insert(stack, i);
                i
            }
        };
        self.resolved_stacks.insert(iid, idx);
        Ok(idx)
    }
}

/// Decodes a `Trace` holding CPU samples. Interning restarts whenever a packet has
/// `sequence_flags & 1`; functions are deduplicated by `(name, library)` across the whole input.
/// Truncated or malformed input is an error; unknown fields are skipped. A sample with an unknown
/// callstack, frame or function-name iid is skipped and counted in [`CpuProfile::skipped_samples`].
pub fn decode_cpu_samples(bytes: &[u8]) -> Result<CpuProfile, String> {
    let mut state = Interned::default();
    let mut out = Builder::default();
    for (n, w) in fields(bytes)? {
        let (1, Wire::Bytes(packet)) = (n, w) else {
            continue;
        };
        let (mut ts, mut flags) = (0u64, 0u64);
        let (mut interned, mut sample) = (None, None);
        for (n, w) in fields(packet)? {
            match (n, w) {
                (8, w) => ts = num(w),
                (13, w) => flags = num(w),
                (12, Wire::Bytes(b)) => interned = Some(b),
                (66, Wire::Bytes(b)) => sample = Some(b),
                _ => {}
            }
        }
        // The flags come after the interned data on the wire but apply before it.
        if flags & 1 != 0 {
            state = Interned::default();
        }
        if let Some(b) = interned {
            state.absorb(b)?;
        }
        if let Some(b) = sample {
            let (mut tid, mut callstack) = (0u64, 0u64);
            for (n, w) in fields(b)? {
                match n {
                    3 => tid = num(w),
                    4 => callstack = num(w),
                    _ => {}
                }
            }
            // One bad iid costs that sample only, not the hundreds of good ones.
            let Ok(stack) = state.stack_of(callstack, &mut out) else {
                out.profile.skipped_samples += 1;
                continue;
            };
            out.profile.samples.push(PerfSampleRec {
                ts_ns: ts,
                tid: tid as u32,
                stack,
            });
        }
    }
    Ok(out.profile)
}

impl VmService {
    /// `getPerfettoCpuSamples` for `[origin, origin + extent]`. The base64 and protobuf are decoded
    /// on a blocking thread. The window is what the reply reports, which is the span of the
    /// samples returned, not the request.
    pub async fn get_perfetto_cpu_samples(
        &self,
        isolate: &str,
        origin_us: i64,
        extent_us: i64,
        timeout: Duration,
    ) -> Result<(CpuProfile, CpuWindow), RpcError> {
        let raw = self
            .client
            .call_raw_with_timeout(
                "getPerfettoCpuSamples",
                json!({"isolateId": isolate, "timeOriginMicros": origin_us,
                       "timeExtentMicros": extent_us}),
                timeout,
            )
            .await?;
        tokio::task::spawn_blocking(move || {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Reply {
                samples: String,
                #[serde(default)]
                sample_period: i64,
                #[serde(default)]
                time_origin_micros: i64,
                #[serde(default)]
                time_extent_micros: i64,
            }
            let r: Reply = serde_json::from_str(raw.get())
                .map_err(|e| RpcError::Decode(format!("getPerfettoCpuSamples: {e}")))?;
            let bytes = STANDARD
                .decode(r.samples.as_bytes())
                .map_err(|e| RpcError::Decode(format!("getPerfettoCpuSamples base64: {e}")))?;
            let profile = decode_cpu_samples(&bytes)
                .map_err(|e| RpcError::Decode(format!("getPerfettoCpuSamples protobuf: {e}")))?;
            Ok((
                profile,
                CpuWindow {
                    origin_us: r.time_origin_micros,
                    extent_us: r.time_extent_micros,
                    period_us: r.sample_period,
                },
            ))
        })
        .await
        .map_err(|e| RpcError::Decode(format!("getPerfettoCpuSamples: {e}")))?
    }
}

/// A minimal encoder for tests, in the layout the VM writes.
#[cfg(any(test, feature = "testing"))]
pub mod encode {
    /// One `getPerfettoCpuSamples` response.
    pub struct Response<'a> {
        /// `(name, library)`; `None` is a native frame.
        pub functions: &'a [(&'a str, Option<&'a str>)],
        /// Root first; indices into `functions`.
        pub stacks: &'a [&'a [usize]],
        /// `(timestamp ns, tid, index into stacks)`.
        pub samples: &'a [(u64, u32, usize)],
    }

    fn varint(out: &mut Vec<u8>, mut v: u64) {
        while v >= 0x80 {
            out.push((v as u8 & 0x7f) | 0x80);
            v >>= 7;
        }
        out.push(v as u8);
    }

    fn uint(out: &mut Vec<u8>, field: u32, v: u64) {
        varint(out, u64::from(field) << 3);
        varint(out, v);
    }

    fn bytes(out: &mut Vec<u8>, field: u32, b: &[u8]) {
        varint(out, (u64::from(field) << 3) | 2);
        varint(out, b.len() as u64);
        out.extend_from_slice(b);
    }

    fn packet(out: &mut Vec<u8>, body: &[u8]) {
        bytes(out, 1, body);
    }

    fn interned_string(iid: u64, s: &str) -> Vec<u8> {
        let mut b = Vec::new();
        uint(&mut b, 1, iid);
        bytes(&mut b, 2, s.as_bytes());
        b
    }

    /// Concatenated responses, like DDS's `getPerfettoVMTimelineWithCpuSamples`. Each one clears
    /// the interning state, so its iids restart at 1.
    pub fn cpu_samples(responses: &[Response]) -> Vec<u8> {
        let mut out = Vec::new();
        for r in responses {
            // A clock snapshot, which the decoder ignores.
            let mut clock = Vec::new();
            uint(&mut clock, 1, 3);
            uint(&mut clock, 2, 1);
            let mut snap = Vec::new();
            bytes(&mut snap, 1, &clock);
            let mut p = Vec::new();
            bytes(&mut p, 6, &snap);
            packet(&mut out, &p);

            let mut libs: Vec<&str> = Vec::new();
            let mut data = Vec::new();
            for (i, (name, lib)) in r.functions.iter().enumerate() {
                bytes(&mut data, 5, &interned_string(i as u64 + 1, name));
                let mut frame = Vec::new();
                uint(&mut frame, 1, i as u64 + 1);
                uint(&mut frame, 2, i as u64 + 1);
                if let Some(lib) = lib {
                    let m = match libs.iter().position(|l| l == lib) {
                        Some(m) => m,
                        None => {
                            libs.push(lib);
                            libs.len() - 1
                        }
                    };
                    uint(&mut frame, 3, m as u64 + 1);
                }
                bytes(&mut data, 6, &frame);
            }
            for (m, lib) in libs.iter().enumerate() {
                bytes(&mut data, 17, &interned_string(m as u64 + 1, lib));
                let mut mapping = Vec::new();
                uint(&mut mapping, 1, m as u64 + 1);
                uint(&mut mapping, 7, m as u64 + 1);
                bytes(&mut data, 19, &mapping);
            }
            let mut p = Vec::new();
            bytes(&mut p, 12, &data);
            uint(&mut p, 13, 3);
            packet(&mut out, &p);

            let mut seen = vec![false; r.stacks.len()];
            for &(ts, tid, stack) in r.samples {
                let mut p = Vec::new();
                uint(&mut p, 8, ts);
                uint(&mut p, 10, 1);
                if !seen[stack] {
                    seen[stack] = true;
                    let mut cs = Vec::new();
                    uint(&mut cs, 1, stack as u64 + 1);
                    for &f in r.stacks[stack] {
                        uint(&mut cs, 2, f as u64 + 1);
                    }
                    let mut d = Vec::new();
                    bytes(&mut d, 7, &cs);
                    bytes(&mut p, 12, &d);
                }
                let mut ps = Vec::new();
                uint(&mut ps, 2, 1);
                uint(&mut ps, 3, u64::from(tid));
                uint(&mut ps, 4, stack as u64 + 1);
                bytes(&mut p, 66, &ps);
                uint(&mut p, 13, 2);
                packet(&mut out, &p);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::encode::{Response, cpu_samples};
    use super::*;

    fn f(name: &str, lib: Option<&str>) -> PerfFunction {
        PerfFunction {
            name: name.into(),
            library: lib.map(Into::into),
        }
    }

    fn sample_bytes() -> Vec<u8> {
        cpu_samples(&[
            Response {
                functions: &[
                    ("main", Some("file:///app/main.dart")),
                    ("build", Some("package:app/a.dart")),
                    ("mach_continuous_time", None),
                ],
                stacks: &[&[0, 1, 2], &[0, 1]],
                samples: &[(2_000, 259, 0), (1_000, 259, 1), (3_000, 7, 0)],
            },
            // The iids restart here and mean different things.
            Response {
                functions: &[
                    ("zzz", None),
                    ("build", Some("package:app/a.dart")),
                    ("main", Some("file:///app/main.dart")),
                ],
                stacks: &[&[2, 1, 0]],
                samples: &[(9_000, 259, 0)],
            },
        ])
    }

    #[test]
    fn cpu_samples_round_trip() {
        let p = decode_cpu_samples(&sample_bytes()).unwrap();
        assert_eq!(
            p.functions,
            vec![
                f("main", Some("file:///app/main.dart")),
                f("build", Some("package:app/a.dart")),
                f("mach_continuous_time", None),
                f("zzz", None),
            ]
        );
        // Root first, and the second response's stack deduplicates onto the first's.
        assert_eq!(p.stacks, vec![vec![0, 1, 2], vec![0, 1], vec![0, 1, 3]]);
        assert_eq!(
            p.samples,
            vec![
                PerfSampleRec {
                    ts_ns: 2_000,
                    tid: 259,
                    stack: 0
                },
                PerfSampleRec {
                    ts_ns: 1_000,
                    tid: 259,
                    stack: 1
                },
                PerfSampleRec {
                    ts_ns: 3_000,
                    tid: 7,
                    stack: 0
                },
                PerfSampleRec {
                    ts_ns: 9_000,
                    tid: 259,
                    stack: 2
                },
            ]
        );
        assert_eq!(p.functions[2].library, None);
    }

    #[test]
    fn empty_input_is_an_empty_profile() {
        assert_eq!(decode_cpu_samples(&[]).unwrap(), CpuProfile::default());
    }

    #[test]
    fn unknown_fields_are_skipped() {
        let mut bytes = sample_bytes();
        // A top-level varint field 99 and a fixed64 field 98.
        bytes.extend_from_slice(&[0x98, 0x06, 0x01]);
        bytes.extend_from_slice(&[0x91, 0x06, 1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(
            decode_cpu_samples(&bytes).unwrap(),
            decode_cpu_samples(&sample_bytes()).unwrap()
        );
    }

    #[test]
    fn truncated_protobuf_is_an_error() {
        let bytes = sample_bytes();
        for cut in [1, 2, bytes.len() / 2, bytes.len() - 1] {
            let r = decode_cpu_samples(&bytes[..bytes.len() - cut]);
            assert!(r.is_err(), "cut {cut} decoded");
        }
        // A length that runs past the end, and a dangling varint.
        assert!(decode_cpu_samples(&[0x0a, 0x7f, 1]).is_err());
        assert!(decode_cpu_samples(&[0x0a]).is_err());
        assert!(
            decode_cpu_samples(&[
                0x0a, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f
            ])
            .is_err()
        );
    }

    /// A perf_sample packet (timestamp `ts`) whose callstack iid is `iid`.
    fn sample_packet(ts: u8, iid: u8) -> Vec<u8> {
        let sample = [0x20, iid];
        let mut packet = vec![0x40, ts, 0x92, 0x04, sample.len() as u8];
        packet.extend_from_slice(&sample);
        let mut trace = vec![0x0a, packet.len() as u8];
        trace.extend_from_slice(&packet);
        trace
    }

    #[test]
    fn samples_with_unknown_ids_are_skipped_and_counted() {
        // A callstack iid that is never defined, ahead of a good response.
        let mut trace = sample_packet(1, 5);
        trace.extend(sample_bytes());
        trace.extend(sample_packet(2, 6));
        let p = decode_cpu_samples(&trace).unwrap();
        assert_eq!(p.skipped_samples, 2);
        assert_eq!(p.samples.len(), 3 + 1, "the good samples survive");
        // A frame that is not defined: a callstack naming it loses only its samples.
        let bad = {
            let mut cs = vec![0x08, 0x01, 0x10, 0x63]; // iid 1, frame 99
            let mut d = vec![0x3a, cs.len() as u8];
            d.append(&mut cs);
            let mut sample = vec![0x20, 0x01];
            let mut packet = vec![0x40, 3, 0x62, d.len() as u8];
            packet.extend_from_slice(&d);
            packet.extend_from_slice(&[0x92, 0x04, sample.len() as u8]);
            packet.append(&mut sample);
            let mut t = vec![0x0a, packet.len() as u8];
            t.extend_from_slice(&packet);
            t
        };
        let p = decode_cpu_samples(&bad).unwrap();
        assert_eq!((p.samples.len(), p.skipped_samples), (0, 1));
    }

    #[tokio::test]
    async fn get_perfetto_cpu_samples_decodes_the_reply() {
        use crate::rpc::{ConnectOptions, connect};
        use crate::testing::ScriptedServer;
        let b64 = STANDARD.encode(sample_bytes());
        let server = ScriptedServer::start(move |m, _| match m {
            "getPerfettoCpuSamples" => Ok(json!({"type":"PerfettoCpuSamples","samplePeriod":1000,
                "sampleCount":4,"timeOriginMicros":10,"timeExtentMicros":99,"samples":b64})),
            _ => Err((-32601, "nope".into())),
        });
        let (client, _n) = connect(&server.ws_uri(), ConnectOptions::default())
            .await
            .unwrap();
        let svc = VmService::new(client);
        let (p, w) = svc
            .get_perfetto_cpu_samples("isolates/1", 5, 100, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(p.samples.len(), 4);
        assert_eq!(
            w,
            CpuWindow {
                origin_us: 10,
                extent_us: 99,
                period_us: 1000
            }
        );
        let got = server.received();
        assert_eq!(got[0].1["isolateId"], "isolates/1");
        assert_eq!(got[0].1["timeOriginMicros"], 5);
        assert_eq!(got[0].1["timeExtentMicros"], 100);
    }
}
