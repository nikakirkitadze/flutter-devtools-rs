//! The phase breakdown of one frame.

use fdt_store::frames::{FrameRecord, FrameStatus, frame_status};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Build,
    Layout,
    Paint,
    Composite,
    Finalize,
    OtherUi,
    Raster,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
    pub phase: Phase,
    pub us: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Breakdown {
    pub segments: Vec<Segment>,
    pub ui_us: u32,
    pub raster_us: u32,
    pub budget_us: u32,
    pub status: FrameStatus,
    pub over: Option<Phase>,
    pub has_timeline: bool,
}

pub fn breakdown(frame: &FrameRecord, budget_us: u32) -> Breakdown {
    let status = frame_status(frame.time_us(), budget_us);
    let seg = |phase, us| Segment { phase, us };
    let (mut segments, ui_us, has_timeline) = match frame.ui {
        Some(u) => {
            let sum = [
                u.build_us,
                u.layout_us,
                u.paint_us,
                u.composite_us,
                u.finalize_us,
            ]
            .iter()
            .fold(0u32, |a, v| a.saturating_add(*v));
            let mut v = vec![
                seg(Phase::Build, u.build_us),
                seg(Phase::Layout, u.layout_us),
                seg(Phase::Paint, u.paint_us),
                seg(Phase::Composite, u.composite_us),
                seg(Phase::Finalize, u.finalize_us),
            ];
            // Shown only when something is left over.
            v.push(seg(Phase::OtherUi, u.ui_us.saturating_sub(sum)));
            (v, u.ui_us, true)
        }
        None => (
            vec![seg(Phase::Build, frame.build_us)],
            frame.build_us,
            false,
        ),
    };
    let ui_segments = segments.clone();
    segments.push(seg(Phase::Raster, frame.raster_us));
    let over = if status == FrameStatus::Ok {
        None
    } else if frame.raster_us > budget_us && frame.raster_us >= ui_us {
        Some(Phase::Raster)
    } else {
        // The first of equal segments wins; a zero-length one is never "over".
        ui_segments
            .iter()
            .fold(None::<Segment>, |best, s| match best {
                Some(b) if b.us >= s.us => Some(b),
                _ => Some(*s),
            })
            .map(|s| s.phase)
    };
    // `OtherUi` is listed only when it is positive.
    segments.retain(|s| s.phase != Phase::OtherUi || s.us > 0);
    Breakdown {
        segments,
        ui_us,
        raster_us: frame.raster_us,
        budget_us,
        status,
        over,
        has_timeline,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fdt_store::frames::{FrameInput, UiPhases};
    use fdt_store::time::{RunId, SessionTime};

    const B: u32 = 16_667;

    /// A frame with UI phases `[build, layout, paint, composite, finalize]` in a UI thread of
    /// `ui_us`, and a raster thread of `raster_us`.
    fn fr(phases: Option<([u32; 5], u32)>, build: u32, raster: u32) -> FrameRecord {
        let (mut w, _) = crate::testkit::writer();
        let run = RunId(1);
        w.push_frame(FrameInput {
            run,
            number: 1,
            start: SessionTime(0),
            elapsed_us: build.max(raster),
            build_us: build,
            raster_us: raster,
            vsync_overhead_us: 0,
        });
        if let Some(([b, l, p, c, f], ui_us)) = phases {
            w.set_frame_ui(
                run,
                1,
                UiPhases {
                    begin: SessionTime(0),
                    ui_us,
                    build_us: b,
                    layout_us: l,
                    paint_us: p,
                    composite_us: c,
                    finalize_us: f,
                    widgets_built: None,
                },
            );
        }
        w.publish().frames.get(0).unwrap().clone()
    }

    #[test]
    fn breakdown_cases() {
        // Raster over.
        let b = breakdown(
            &fr(Some(([3000, 1000, 1000, 500, 500], 6000)), 6000, 40_000),
            B,
        );
        assert_eq!(b.status, FrameStatus::Janky);
        assert_eq!(b.over, Some(Phase::Raster));
        assert!(b.has_timeline);
        // A zero OtherUi is not listed.
        assert_eq!(b.segments.len(), 6);
        assert!(b.segments.iter().all(|s| s.phase != Phase::OtherUi));
        // Build over.
        let b = breakdown(
            &fr(Some(([25_000, 1000, 1000, 500, 500], 28_000)), 28_000, 4000),
            B,
        );
        assert_eq!(b.over, Some(Phase::Build));
        assert!(b.segments.iter().all(|s| s.phase != Phase::OtherUi));
        // Layout over.
        let b = breakdown(
            &fr(Some(([1000, 20_000, 1000, 500, 500], 23_000)), 23_000, 4000),
            B,
        );
        assert_eq!(b.over, Some(Phase::Layout));
        // Other UI over.
        let b = breakdown(
            &fr(Some(([1000, 1000, 1000, 500, 500], 24_000)), 24_000, 4000),
            B,
        );
        assert_eq!(b.over, Some(Phase::OtherUi));
        assert_eq!(
            b.segments[5],
            Segment {
                phase: Phase::OtherUi,
                us: 20_000
            }
        );
        assert_eq!(b.ui_us, 24_000);
        assert_eq!(b.raster_us, 4000);
        // Ok has no over.
        let b = breakdown(
            &fr(Some(([3000, 1000, 1000, 500, 500], 6000)), 6000, 5000),
            B,
        );
        assert_eq!((b.status, b.over), (FrameStatus::Ok, None));
        // No phases.
        let b = breakdown(&fr(None, 30_000, 4000), B);
        assert!(!b.has_timeline);
        assert_eq!(
            b.segments,
            vec![
                Segment {
                    phase: Phase::Build,
                    us: 30_000
                },
                Segment {
                    phase: Phase::Raster,
                    us: 4000
                }
            ]
        );
        assert_eq!(b.over, Some(Phase::Build));
    }
}
