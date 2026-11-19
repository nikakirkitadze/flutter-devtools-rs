//! `Flutter.Frame` extension events.

use fdt_store::frames::FrameInput;
use serde_json::Value;

use super::Sink;

fn us(v: &Value, key: &str) -> u32 {
    v.get(key)
        .and_then(Value::as_i64)
        .map_or(0, |n| n.clamp(0, u32::MAX as i64) as u32)
}

pub(crate) fn frame_input(sink: &Sink, data: &Value) -> Option<FrameInput> {
    let start = data.get("startTime").and_then(Value::as_i64)?;
    Some(FrameInput {
        run: sink.run,
        number: data.get("number").and_then(Value::as_u64).unwrap_or(0),
        start: sink.clock.from_timeline_us(start).time,
        elapsed_us: us(data, "elapsed"),
        build_us: us(data, "build"),
        raster_us: us(data, "raster"),
        vsync_overhead_us: us(data, "vsyncOverhead"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fdt_store::clock::{Calibration, RunClock};
    use fdt_store::time::{RunId, SessionTime};
    use serde_json::json;
    use tokio::sync::mpsc;

    #[test]
    fn frame_times_map_through_the_timeline_clock() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let clock = RunClock::new(
            1_000_000,
            Calibration {
                skew_us: 0,
                timeline_to_wall_us: 1_000_000,
            },
        );
        let sink = Sink::new(RunId(1), clock, 1, tx);
        let f = frame_input(
            &sink,
            &json!({"number": 2, "startTime": 500, "elapsed": 48615, "build": 3180, "raster": 15895, "vsyncOverhead": 29501}),
        )
        .unwrap();
        assert_eq!(f.start, SessionTime(500_000));
        assert_eq!(
            (f.number, f.elapsed_us, f.build_us, f.raster_us),
            (2, 48615, 3180, 15895)
        );
        assert_eq!(f.vsync_overhead_us, 29501);
        assert!(frame_input(&sink, &json!({"number": 1})).is_none());
    }
}
