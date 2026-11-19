//! Typed `streamNotify` events.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::Deserialize;
use serde_json::Value;

use crate::rpc::Notification;
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
    Other {
        stream: String,
        kind: String,
    },
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
            _ => return None,
        })
    })();
    Some(decoded.unwrap_or_else(|| StreamEvent::Other {
        stream: stream.to_string(),
        kind: kind.to_string(),
    }))
}
