//! Shared fixtures for the unit tests.

use fdt_store::SessionWriter;
use fdt_store::clock::{Calibration, RunClock};
use fdt_store::frames::FrameInput;
use fdt_store::net::HttpUpdate;
use fdt_store::run::{AppInfo, Mode};
use fdt_store::time::{RunId, SessionTime};

pub const MS: u64 = 1_000_000;

pub fn writer() -> (SessionWriter, RunId) {
    let mut w = SessionWriter::new(0);
    let app = AppInfo {
        name: "demo".into(),
        device: None,
        mode: Mode::Debug,
        flutter_version: None,
        workspace: None,
        vm_uri: "ws://127.0.0.1:1/ws".into(),
        pid: None,
    };
    let clock = RunClock::new(
        0,
        Calibration {
            skew_us: 0,
            timeline_to_wall_us: 0,
        },
    );
    let run = w.start_run(app, clock, SessionTime(0));
    (w, run)
}

pub fn frame(w: &mut SessionWriter, run: RunId, number: u64, start_ms: u64, elapsed_us: u32) {
    w.push_frame(FrameInput {
        run,
        number,
        start: SessionTime(start_ms * MS),
        elapsed_us,
        build_us: elapsed_us / 2,
        raster_us: elapsed_us / 2,
        vsync_overhead_us: 0,
    });
}

pub fn http(run: RunId, id: &str, start_ms: u64, end_ms: Option<u64>) -> HttpUpdate {
    HttpUpdate {
        run,
        isolate: "i".into(),
        vm_id: id.into(),
        method: "GET".into(),
        url: format!("http://h/{id}"),
        start: SessionTime(start_ms * MS),
        start_clamped: false,
        request_sent: None,
        response_start: None,
        end: end_ms.map(|m| SessionTime(m * MS)),
        status: end_ms.map(|_| 200),
        reason: None,
        request_headers: Vec::new(),
        response_headers: Vec::new(),
        request_content_length: None,
        response_content_length: None,
        events: Vec::new(),
        error: None,
        websocket: false,
        remote: None,
    }
}
