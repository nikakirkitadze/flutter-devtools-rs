//! Turning stream events into session records.
//!
//! Connection tasks never touch the store; they send [`Ingest`] messages to the session task.

pub(crate) mod flutter_error;
pub(crate) mod frames;
pub(crate) mod logs;
pub(crate) mod stdout;

use std::time::Duration;

use fdt_store::clock::{Calibration, RunClock};
use fdt_store::frames::FrameInput;
use fdt_store::logs::{LogInput, LogLevel, LogSource};
use fdt_store::run::AppInfo;
use fdt_store::time::RunId;
use fdt_vm::events::{StreamEvent, WriteStream};
use fdt_vm::service::VmService;
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use crate::capture::SessionMsg;
use stdout::QuietLines;

/// What a connection task asks the session task to store.
pub(crate) enum Ingest {
    /// The session creates its store at the first of these, so its origin is the first success.
    StartRun {
        app: Box<AppInfo>,
        calibration: Calibration,
        reply: oneshot::Sender<(RunId, RunClock)>,
    },
    /// The connect sequence failed.
    Failed {
        message: String,
        details: String,
    },
    Log(LogInput),
    Frame(FrameInput),
    FlutterVersion {
        run: RunId,
        version: String,
    },
}

/// One run's channel to the session task, plus what is needed to map event times.
#[derive(Clone)]
pub(crate) struct Sink {
    pub(crate) run: RunId,
    pub(crate) clock: RunClock,
    conn: u64,
    tx: mpsc::UnboundedSender<SessionMsg>,
}

impl Sink {
    pub(crate) fn new(
        run: RunId,
        clock: RunClock,
        conn: u64,
        tx: mpsc::UnboundedSender<SessionMsg>,
    ) -> Sink {
        Sink {
            run,
            clock,
            conn,
            tx,
        }
    }

    pub(crate) fn send(&self, i: Ingest) {
        let _ = self.tx.send(SessionMsg::Ingest(self.conn, i));
    }

    pub(crate) fn log(&self, time_ms: i64, level: LogLevel, source: LogSource, text: String) {
        let st = self.clock.from_epoch_ms(time_ms);
        self.send(Ingest::Log(LogInput {
            run: self.run,
            time: st.time,
            clamped: st.clamped,
            level,
            source,
            logger: String::new(),
            text,
            error: None,
        }));
    }
}

/// Routes decoded events of one run. Owned by the connection task.
pub(crate) struct Router {
    sink: Sink,
    svc: VmService,
    main_isolate: String,
    logging: mpsc::UnboundedSender<logs::LoggingEvent>,
    stdout: QuietLines,
    stderr: QuietLines,
    last_ts: [i64; 2],
}

impl Router {
    pub(crate) fn new(
        sink: Sink,
        svc: VmService,
        main_isolate: String,
        stdout_flush: Duration,
        string_fetch_timeout: Duration,
    ) -> Router {
        let logging = logs::spawn_worker(sink.clone(), svc.clone(), string_fetch_timeout);
        Router {
            sink,
            svc,
            main_isolate,
            logging,
            stdout: QuietLines::new(stdout_flush),
            stderr: QuietLines::new(stdout_flush),
            last_ts: [0; 2],
        }
    }

    pub(crate) fn on_event(&mut self, ev: StreamEvent) {
        match ev {
            StreamEvent::Logging {
                isolate,
                time_ms,
                level,
                logger,
                message,
                error,
                stack_trace,
                ..
            } => {
                let _ = self.logging.send(logs::LoggingEvent {
                    isolate: isolate.id,
                    time_ms,
                    level,
                    logger,
                    message,
                    error,
                    stack_trace,
                });
            }
            StreamEvent::Write {
                stream,
                bytes,
                timestamp_ms,
            } => {
                let (lines, level, source, slot) = match stream {
                    WriteStream::Stdout => (&mut self.stdout, LogLevel::Info, LogSource::Stdout, 0),
                    WriteStream::Stderr => {
                        (&mut self.stderr, LogLevel::Error, LogSource::Stderr, 1)
                    }
                };
                self.last_ts[slot] = timestamp_ms;
                for line in lines.push(&bytes, Instant::now()) {
                    self.sink.log(timestamp_ms, level, source, line);
                }
            }
            StreamEvent::Extension {
                kind,
                data,
                timestamp_ms,
                ..
            } => self.on_extension(&kind, &data, timestamp_ms),
            StreamEvent::ServiceRegistered { method, .. } if method.ends_with("flutterVersion") => {
                let svc = self.svc.clone();
                let isolate = self.main_isolate.clone();
                let sink = self.sink.clone();
                tokio::spawn(async move {
                    match svc.flutter_version(&method, &isolate).await {
                        Ok(v) => {
                            if let Some(version) = v.flutter_version.or(v.framework_version) {
                                sink.send(Ingest::FlutterVersion {
                                    run: sink.run,
                                    version,
                                });
                            }
                        }
                        Err(e) => log::debug!("flutterVersion failed: {e}"),
                    }
                });
            }
            _ => {}
        }
    }

    fn on_extension(&mut self, kind: &str, data: &Value, timestamp_ms: i64) {
        match kind {
            "Flutter.Error" => {
                let st = self.sink.clock.from_epoch_ms(timestamp_ms);
                self.sink
                    .send(Ingest::Log(flutter_error::record(self.sink.run, st, data)));
            }
            "Flutter.Navigation" => {
                if let Some(text) = navigation_text(data) {
                    self.sink
                        .log(timestamp_ms, LogLevel::Info, LogSource::Navigation, text);
                }
            }
            "Flutter.Frame" => {
                if let Some(f) = frames::frame_input(&self.sink, data) {
                    self.sink.send(Ingest::Frame(f));
                }
            }
            _ => {}
        }
    }

    /// When a buffered partial stdout or stderr line is next due.
    pub(crate) fn next_flush(&self) -> Option<Instant> {
        [self.stdout.deadline(), self.stderr.deadline()]
            .into_iter()
            .flatten()
            .min()
    }

    pub(crate) fn flush_due(&mut self, now: Instant) {
        if let Some(line) = self.stdout.flush_due(now) {
            self.sink
                .log(self.last_ts[0], LogLevel::Info, LogSource::Stdout, line);
        }
        if let Some(line) = self.stderr.flush_due(now) {
            self.sink
                .log(self.last_ts[1], LogLevel::Error, LogSource::Stderr, line);
        }
    }
}

fn navigation_text(data: &Value) -> Option<String> {
    let route = data.get("route")?;
    let name = route
        .get("settings")
        .and_then(|s| s.get("name"))
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
        .or_else(|| route.get("description").and_then(Value::as_str))?;
    Some(format!("Navigated to {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn navigation_prefers_the_settings_name() {
        let d = json!({"route": {"description": "MaterialPageRoute<void>(/logs)", "settings": {"name": "/logs"}}});
        assert_eq!(navigation_text(&d).unwrap(), "Navigated to /logs");
        let d = json!({"route": {"description": "SomeRoute"}});
        assert_eq!(navigation_text(&d).unwrap(), "Navigated to SomeRoute");
        assert_eq!(navigation_text(&json!({})), None);
    }
}
