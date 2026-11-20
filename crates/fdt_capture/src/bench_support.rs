//! A way in for the `ingest` benchmark and the budget test, which cannot see crate-private items.
//! Not part of the public API.

use fdt_vm::timeline::decode_trace_events;

use crate::ingest::assemble::{AssembledBatch, Assembler};

/// Events per synthetic frame.
pub const EVENTS_PER_FRAME: usize = 70;

#[allow(clippy::too_many_arguments)]
fn push(
    s: &mut String,
    ph: &str,
    cat: &str,
    name: &str,
    tid: i64,
    ts: i64,
    id: Option<u32>,
    frame: Option<u64>,
) {
    use std::fmt::Write;
    write!(
        s,
        r#"{{"name":"{name}","cat":"{cat}","ph":"{ph}","pid":1000,"tid":{tid},"ts":{ts}"#
    )
    .unwrap();
    if let Some(id) = id {
        write!(s, r#","id":"{id:x}""#).unwrap();
    }
    s.push_str(r#","args":{"#);
    if let Some(f) = frame {
        write!(s, r#""frame_number":"{f}","#).unwrap();
    }
    s.push_str(r#""isolateGroupId":"isolateGroups/7801228836854853","isolateId":"isolates/2057427926276255"}}"#);
}

/// A `getVMTimeline`-shaped reply with `frames` frames of exactly [`EVENTS_PER_FRAME`] events
/// each, shaped like a real frame: vsync, `BeginFrame` with its phases and 15 widget slices on the
/// UI thread, a raster draw, and two async slices.
pub fn synthetic_timeline_json(frames: usize) -> String {
    let mut s = String::with_capacity(frames * EVENTS_PER_FRAME * 190);
    s.push_str(r#"{"type":"Timeline","traceEvents":["#);
    let mut first = true;
    let mut ev = |s: &mut String, ph, cat, name, tid, ts, id, fr| {
        if !std::mem::take(&mut first) {
            s.push(',');
        }
        push(s, ph, cat, name, tid, ts, id, fr);
    };
    const UI: i64 = 259;
    const RASTER: i64 = 300;
    for f in 0..frames {
        let n = 1000 + f as u64;
        let mut t = 1_000_000 + f as i64 * 16_667;
        let mut tick = || {
            t += 7;
            t
        };
        ev(
            &mut s,
            "B",
            "Embedder",
            "VsyncProcessCallback",
            UI,
            tick(),
            None,
            None,
        );
        ev(
            &mut s,
            "B",
            "Embedder",
            "Animator::BeginFrame",
            UI,
            tick(),
            None,
            Some(n),
        );
        ev(&mut s, "B", "Dart", "BUILD", UI, tick(), None, None);
        for _ in 0..4 {
            for w in ["Scaffold", "Padding", "Text"] {
                ev(&mut s, "B", "Dart", w, UI, tick(), None, None);
            }
            for w in ["Text", "Padding", "Scaffold"] {
                ev(&mut s, "E", "Dart", w, UI, tick(), None, None);
            }
        }
        for _ in 0..3 {
            ev(&mut s, "B", "Dart", "Center", UI, tick(), None, None);
            ev(&mut s, "E", "Dart", "Center", UI, tick(), None, None);
        }
        ev(&mut s, "E", "Dart", "BUILD", UI, tick(), None, None);
        for (outer, inner) in [
            ("LAYOUT (root)", Some("LAYOUT")),
            (
                "UPDATING COMPOSITING BITS (root)",
                Some("UPDATING COMPOSITING BITS"),
            ),
            ("PAINT (root)", Some("PAINT")),
            ("COMPOSITING", None),
            ("FINALIZE TREE", None),
            ("POST_FRAME", None),
        ] {
            ev(&mut s, "B", "Dart", outer, UI, tick(), None, None);
            if let Some(i) = inner {
                ev(&mut s, "B", "Dart", i, UI, tick(), None, None);
                ev(&mut s, "E", "Dart", i, UI, tick(), None, None);
            }
            ev(&mut s, "E", "Dart", outer, UI, tick(), None, None);
        }
        ev(
            &mut s,
            "E",
            "Embedder",
            "Animator::BeginFrame",
            UI,
            tick(),
            None,
            None,
        );
        ev(
            &mut s,
            "E",
            "Embedder",
            "VsyncProcessCallback",
            UI,
            tick(),
            None,
            None,
        );
        let id = (n % 1000) as u32;
        ev(
            &mut s,
            "b",
            "Embedder",
            "PipelineItem",
            UI,
            tick(),
            Some(id),
            None,
        );
        ev(
            &mut s,
            "b",
            "Embedder",
            "PipelineProduce",
            UI,
            tick(),
            Some(id),
            None,
        );
        ev(
            &mut s,
            "e",
            "Embedder",
            "PipelineProduce",
            UI,
            tick(),
            Some(id),
            None,
        );
        ev(
            &mut s,
            "e",
            "Embedder",
            "PipelineItem",
            UI,
            tick(),
            Some(id),
            None,
        );
        ev(
            &mut s,
            "B",
            "Embedder",
            "GPURasterizer::Draw",
            RASTER,
            tick(),
            None,
            None,
        );
        ev(
            &mut s,
            "B",
            "Embedder",
            "Rasterizer::DoDraw",
            RASTER,
            tick(),
            None,
            Some(n),
        );
        ev(
            &mut s,
            "B",
            "Embedder",
            "Rasterizer::DrawToSurfaces",
            RASTER,
            tick(),
            None,
            None,
        );
        ev(
            &mut s,
            "B",
            "Embedder",
            "LayerTree::Paint",
            RASTER,
            tick(),
            None,
            None,
        );
        ev(
            &mut s,
            "E",
            "Embedder",
            "LayerTree::Paint",
            RASTER,
            tick(),
            None,
            None,
        );
        ev(
            &mut s,
            "E",
            "Embedder",
            "Rasterizer::DrawToSurfaces",
            RASTER,
            tick(),
            None,
            None,
        );
        ev(
            &mut s,
            "E",
            "Embedder",
            "Rasterizer::DoDraw",
            RASTER,
            tick(),
            None,
            None,
        );
        ev(
            &mut s,
            "E",
            "Embedder",
            "GPURasterizer::Draw",
            RASTER,
            tick(),
            None,
            None,
        );
        ev(
            &mut s,
            "b",
            "Dart",
            "Frame",
            UI,
            tick(),
            Some(900_000 + id),
            None,
        );
        ev(
            &mut s,
            "b",
            "Dart",
            "Animate",
            UI,
            tick(),
            Some(900_000 + id),
            None,
        );
        ev(
            &mut s,
            "e",
            "Dart",
            "Animate",
            UI,
            tick(),
            Some(900_000 + id),
            None,
        );
        ev(
            &mut s,
            "e",
            "Dart",
            "Frame",
            UI,
            tick(),
            Some(900_000 + id),
            None,
        );
    }
    s.push_str("]}");
    s
}

/// Decodes the JSON and assembles it in one feed. Returns the slice count.
pub fn decode_and_assemble(json: &str) -> usize {
    let (events, _) = decode_trace_events(json);
    let mut a = Assembler::new();
    let mut out = AssembledBatch::default();
    a.feed(events, &mut out);
    out.slices.len()
}
