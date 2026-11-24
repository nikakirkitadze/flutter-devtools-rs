//! Typed `streamNotify` events.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde_json::Value;

use crate::rpc::Notification;
use crate::timeline::{TraceEvent, decode_trace_values};
use crate::types::{InstanceRef, IsolateRef};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteStream {
    Stdout,
    Stderr,
}

// Logging carries four instances by value so consumers can destructure it directly; events are
// decoded and moved once, never stored in bulk.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, PartialEq)]
pub enum StreamEvent {
    Logging {
        isolate: IsolateRef,
        timestamp_ms: i64,
        sequence: i64,
        time_ms: i64,
        level: i64,
        logger: InstanceRef,
        message: InstanceRef,
        error: InstanceRef,
        stack_trace: InstanceRef,
    },
    Write {
        stream: WriteStream,
        bytes: Vec<u8>,
        timestamp_ms: i64,
    },
    Extension {
        isolate: IsolateRef,
        kind: String,
        data: Value,
        timestamp_ms: i64,
    },
    IsolateStart {
        isolate: IsolateRef,
    },
    IsolateRunnable {
        isolate: IsolateRef,
    },
    IsolateExit {
        isolate: IsolateRef,
    },
    ServiceExtensionAdded {
        isolate: IsolateRef,
        rpc: String,
    },
    IsolateReload {
        isolate_group: Option<String>,
    },
    ServiceRegistered {
        service: String,
        method: String,
    },
    /// A completed block of timeline events (`Timeline` stream).
    TimelineEvents {
        events: Vec<TraceEvent>,
    },
    /// The recorded timeline streams changed (`TimelineStreamSubscriptionsUpdate`).
    TimelineSubscriptions {
        recorded: Vec<String>,
    },
    /// A garbage collection (`GC` stream). One arrives per isolate of the group, per GC.
    Gc(GcEvent),
    Other {
        stream: String,
        kind: String,
    },
}

/// One heap space after a GC. Sizes are bytes; `time_s` is the cumulative GC time in seconds.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HeapSpace {
    pub used: u64,
    pub capacity: u64,
    pub external: u64,
    pub collections: u64,
    pub time_s: f64,
}

impl HeapSpace {
    fn from_json(v: Option<&Value>) -> HeapSpace {
        let n = |k: &str| {
            v.and_then(|v| v.get(k))
                .and_then(Value::as_u64)
                .unwrap_or(0)
        };
        HeapSpace {
            used: n("used"),
            capacity: n("capacity"),
            external: n("external"),
            collections: n("collections"),
            time_s: v
                .and_then(|v| v.get("time"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct GcEvent {
    pub isolate_group: String,
    pub gc_type: String,
    pub reason: Option<String>,
    pub timestamp_ms: i64,
    pub new_space: HeapSpace,
    pub old_space: HeapSpace,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LogRecord {
    #[serde(default)]
    sequence_number: i64,
    #[serde(default)]
    time: i64,
    #[serde(default)]
    level: i64,
    #[serde(default)]
    logger_name: InstanceRef,
    #[serde(default)]
    message: InstanceRef,
    #[serde(default)]
    error: InstanceRef,
    #[serde(default)]
    stack_trace: InstanceRef,
}

fn str_of<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

fn isolate_of(ev: &Value) -> Option<IsolateRef> {
    serde_json::from_value(ev.get("isolate")?.clone()).ok()
}

/// Decodes a `streamNotify` notification. Returns `None` for any other notification.
/// A known stream whose payload does not parse comes back as `Other` rather than being dropped.
pub fn decode_notification(n: &Notification) -> Option<StreamEvent> {
    if n.method != "streamNotify" {
        return None;
    }
    let stream = str_of(&n.params, "streamId")?;
    let ev = n.params.get("event")?;
    let kind = str_of(ev, "kind").unwrap_or_default();
    let ts = ev.get("timestamp").and_then(Value::as_i64).unwrap_or(0);
    let decoded = (|| {
        Some(match kind {
            "Logging" => {
                let r: LogRecord = serde_json::from_value(ev.get("logRecord")?.clone()).ok()?;
                StreamEvent::Logging {
                    isolate: isolate_of(ev)?,
                    timestamp_ms: ts,
                    sequence: r.sequence_number,
                    time_ms: r.time,
                    level: r.level,
                    logger: r.logger_name,
                    message: r.message,
                    error: r.error,
                    stack_trace: r.stack_trace,
                }
            }
            "WriteEvent" => StreamEvent::Write {
                stream: match stream {
                    "Stdout" => WriteStream::Stdout,
                    "Stderr" => WriteStream::Stderr,
                    _ => return None,
                },
                bytes: STANDARD.decode(str_of(ev, "bytes")?).ok()?,
                timestamp_ms: ts,
            },
            "Extension" => StreamEvent::Extension {
                isolate: isolate_of(ev)?,
                kind: str_of(ev, "extensionKind")?.to_string(),
                data: ev.get("extensionData").cloned().unwrap_or(Value::Null),
                timestamp_ms: ts,
            },
            "IsolateStart" => StreamEvent::IsolateStart {
                isolate: isolate_of(ev)?,
            },
            "IsolateRunnable" => StreamEvent::IsolateRunnable {
                isolate: isolate_of(ev)?,
            },
            "IsolateExit" => StreamEvent::IsolateExit {
                isolate: isolate_of(ev)?,
            },
            "ServiceExtensionAdded" => StreamEvent::ServiceExtensionAdded {
                isolate: isolate_of(ev)?,
                rpc: str_of(ev, "extensionRPC")?.to_string(),
            },
            "IsolateReload" => StreamEvent::IsolateReload {
                isolate_group: ev
                    .get("isolateGroup")
                    .and_then(|g| str_of(g, "id"))
                    .map(str::to_string),
            },
            "ServiceRegistered" => StreamEvent::ServiceRegistered {
                service: str_of(ev, "service")?.to_string(),
                method: str_of(ev, "method")?.to_string(),
            },
            "TimelineEvents" => {
                let raw = ev.get("timelineEvents")?.as_array()?;
                let (events, skipped) = decode_trace_values(raw);
                if skipped > 0 {
                    log::debug!("skipped {skipped} unreadable timeline events");
                }
                StreamEvent::TimelineEvents { events }
            }
            "TimelineStreamSubscriptionsUpdate" => StreamEvent::TimelineSubscriptions {
                recorded: ev
                    .get("updatedStreams")?
                    .as_array()?
                    .iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect(),
            },
            "GC" => StreamEvent::Gc(GcEvent {
                isolate_group: ev
                    .get("isolateGroup")
                    .and_then(|g| str_of(g, "id"))
                    .or_else(|| ev.get("isolate").and_then(|i| str_of(i, "isolateGroupId")))?
                    .to_string(),
                gc_type: str_of(ev, "gcType").unwrap_or("GC").to_string(),
                reason: str_of(ev, "reason").map(str::to_string),
                timestamp_ms: ts,
                new_space: HeapSpace::from_json(ev.get("new")),
                old_space: HeapSpace::from_json(ev.get("old")),
            }),
            _ => return None,
        })
    })();
    Some(decoded.unwrap_or_else(|| {
        if KNOWN_KINDS.contains(&kind) {
            log::warn!("could not decode {kind} event on the {stream} stream; keeping it as Other");
        }
        StreamEvent::Other {
            stream: stream.to_string(),
            kind: kind.to_string(),
        }
    }))
}

const KNOWN_KINDS: &[&str] = &[
    "Logging",
    "WriteEvent",
    "Extension",
    "IsolateStart",
    "IsolateRunnable",
    "IsolateExit",
    "ServiceExtensionAdded",
    "IsolateReload",
    "ServiceRegistered",
    "TimelineEvents",
    "TimelineStreamSubscriptionsUpdate",
    "GC",
];

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn note(stream: &str, event: Value) -> Notification {
        Notification {
            method: "streamNotify".into(),
            params: json!({"streamId": stream, "event": event}),
        }
    }

    fn iso() -> Value {
        json!({"id": "isolates/1", "name": "main", "isSystemIsolate": false, "isolateGroupId": "isolateGroups/1"})
    }

    #[test]
    fn isolate_lifecycle_events_decode() {
        for (kind, want) in [
            ("IsolateStart", "start"),
            ("IsolateRunnable", "runnable"),
            ("IsolateExit", "exit"),
        ] {
            let ev = decode_notification(&note(
                "Isolate",
                json!({"kind": kind, "timestamp": 1, "isolate": iso()}),
            ))
            .unwrap();
            match (want, ev) {
                ("start", StreamEvent::IsolateStart { isolate })
                | ("runnable", StreamEvent::IsolateRunnable { isolate })
                | ("exit", StreamEvent::IsolateExit { isolate }) => {
                    assert_eq!(isolate.id, "isolates/1");
                    assert_eq!(isolate.name, "main");
                }
                (_, other) => panic!("{kind} decoded as {other:?}"),
            }
        }
    }

    #[test]
    fn service_extension_added_decodes() {
        let ev = decode_notification(&note(
            "Isolate",
            json!({"kind": "ServiceExtensionAdded", "timestamp": 1, "isolate": iso(),
                   "extensionRPC": "ext.flutter.reassemble"}),
        ))
        .unwrap();
        match ev {
            StreamEvent::ServiceExtensionAdded { isolate, rpc } => {
                assert_eq!(isolate.id, "isolates/1");
                assert_eq!(rpc, "ext.flutter.reassemble");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn isolate_reload_with_and_without_group() {
        let with = decode_notification(&note(
            "Isolate",
            json!({"kind": "IsolateReload", "timestamp": 1, "isolate": iso(),
                   "isolateGroup": {"type": "@IsolateGroup", "id": "isolateGroups/7"}}),
        ))
        .unwrap();
        assert_eq!(
            with,
            StreamEvent::IsolateReload {
                isolate_group: Some("isolateGroups/7".into())
            }
        );
        let without = decode_notification(&note(
            "Isolate",
            json!({"kind": "IsolateReload", "timestamp": 1, "isolate": iso()}),
        ))
        .unwrap();
        assert_eq!(
            without,
            StreamEvent::IsolateReload {
                isolate_group: None
            }
        );
    }

    #[test]
    fn known_kind_that_fails_to_decode_is_other() {
        let ev = decode_notification(&note(
            "Stdout",
            json!({"kind": "WriteEvent", "timestamp": 1, "bytes": "!!not base64!!"}),
        ))
        .unwrap();
        assert_eq!(
            ev,
            StreamEvent::Other {
                stream: "Stdout".into(),
                kind: "WriteEvent".into()
            }
        );
    }

    #[test]
    fn gc_event_decodes() {
        let ev = decode_notification(&note(
            "GC",
            json!({"kind": "GC", "gcType": "Scavenge", "reason": "new space",
                   "timestamp": 1795272383990i64,
                   "isolate": {"id": "isolates/1", "isolateGroupId": "isolateGroups/5"},
                   "isolateGroup": {"id": "isolateGroups/5"},
                   "new": {"type": "HeapSpace", "used": 3264, "capacity": 1048576,
                           "external": 0, "collections": 10, "time": 0.010284},
                   "old": {"type": "HeapSpace", "used": 9331424, "capacity": 10027008,
                           "external": 1984, "collections": 5, "time": 0.00565}}),
        ))
        .unwrap();
        assert_eq!(
            ev,
            StreamEvent::Gc(GcEvent {
                isolate_group: "isolateGroups/5".into(),
                gc_type: "Scavenge".into(),
                reason: Some("new space".into()),
                timestamp_ms: 1795272383990,
                new_space: HeapSpace {
                    used: 3264,
                    capacity: 1048576,
                    external: 0,
                    collections: 10,
                    time_s: 0.010284
                },
                old_space: HeapSpace {
                    used: 9331424,
                    capacity: 10027008,
                    external: 1984,
                    collections: 5,
                    time_s: 0.00565
                },
            })
        );
    }

    #[test]
    fn gc_event_without_gc_type_or_group_object() {
        let ev = decode_notification(&note(
            "GC",
            json!({"kind": "GC", "timestamp": 7,
                   "isolate": {"id": "isolates/1", "isolateGroupId": "isolateGroups/9"},
                   "new": {"used": 5}}),
        ))
        .unwrap();
        let StreamEvent::Gc(g) = ev else {
            panic!("{ev:?}")
        };
        assert_eq!(g.isolate_group, "isolateGroups/9");
        assert_eq!(g.gc_type, "GC");
        assert_eq!(g.reason, None);
        assert_eq!(g.new_space.used, 5);
        assert_eq!(g.new_space.capacity, 0);
        assert_eq!(g.old_space, HeapSpace::default());
    }
}
