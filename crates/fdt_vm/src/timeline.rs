//! The VM timeline: flags, `getVMTimeline` and its JSON trace events (also the payload of the
//! `Timeline` stream's `TimelineEvents`).

use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::rpc::RpcError;
use crate::service::{VmService, decode};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Begin,
    End,
    Complete,
    AsyncBegin,
    AsyncEnd,
    Instant,
    Counter,
    Metadata,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Dart,
    Embedder,
    Gc,
    Other,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TraceEvent {
    pub ph: Phase,
    pub name: Box<str>,
    pub cat: Category,
    pub pid: i64,
    pub tid: i64,
    pub ts_us: i64,
    pub dur_us: Option<i64>,
    pub id: Option<u64>,
    pub frame_number: Option<u64>,
    /// From `M` events named `thread_name`: `args.name`.
    pub thread_name: Option<Box<str>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelineFlags {
    #[serde(default, rename = "availableStreams")]
    pub available: Vec<String>,
    #[serde(default, rename = "recordedStreams")]
    pub recorded: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TimelineChunk {
    pub events: Vec<TraceEvent>,
    /// Events that were not objects or lacked `ph`, or `ts` (other than `M` events, which have none).
    pub skipped: u32,
}

/// The stream set fdt wants recorded: what is recorded now plus `Dart` and `Embedder` when they
/// are available and missing (in that order). `None` when nothing needs to change. Names that are
/// not in `available` are never added, because the VM rejects the whole call for an unknown name.
pub fn wanted_streams(flags: &TimelineFlags) -> Option<Vec<String>> {
    let mut want = flags.recorded.clone();
    let mut changed = false;
    for s in ["Dart", "Embedder"] {
        if flags.available.iter().any(|a| a == s) && !want.iter().any(|w| w == s) {
            want.push(s.to_string());
            changed = true;
        }
    }
    changed.then_some(want)
}

fn parse_id(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        // The VM prints ids as bare hex ("1fdb"). A string that is not valid hex for a u64
        // (too long, e.g. a 19-digit decimal) is read as decimal.
        Value::String(s) => {
            let t = s.trim();
            if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
                return u64::from_str_radix(h, 16).ok();
            }
            u64::from_str_radix(t, 16).ok().or_else(|| t.parse().ok())
        }
        _ => None,
    }
}

fn parse_u64ish(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn micros(v: &Value) -> Option<i64> {
    v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))
}

fn decode_event(v: &Value) -> Option<TraceEvent> {
    let o = v.as_object()?;
    let ph = match o.get("ph")?.as_str()? {
        "B" => Phase::Begin,
        "E" => Phase::End,
        "X" => Phase::Complete,
        "b" => Phase::AsyncBegin,
        "e" => Phase::AsyncEnd,
        "i" | "I" => Phase::Instant,
        "C" => Phase::Counter,
        "M" => Phase::Metadata,
        _ => Phase::Other,
    };
    // Real `M` (metadata) events carry no `ts`; every other kind needs one.
    let ts_us = match o.get("ts") {
        Some(v) => micros(v)?,
        None if ph == Phase::Metadata => 0,
        None => return None,
    };
    let name = o.get("name").and_then(Value::as_str).unwrap_or("");
    let args = o.get("args");
    let thread_name = (ph == Phase::Metadata && name == "thread_name")
        .then(|| args?.get("name")?.as_str().map(Box::from))
        .flatten();
    Some(TraceEvent {
        ph,
        name: name.into(),
        cat: match o.get("cat").and_then(Value::as_str) {
            Some("Dart") => Category::Dart,
            Some("Embedder") => Category::Embedder,
            Some("GC") => Category::Gc,
            _ => Category::Other,
        },
        pid: o.get("pid").and_then(Value::as_i64).unwrap_or(0),
        tid: o.get("tid").and_then(Value::as_i64).unwrap_or(0),
        ts_us,
        dur_us: o.get("dur").and_then(micros),
        id: o.get("id").and_then(parse_id),
        frame_number: args
            .and_then(|a| a.get("frame_number"))
            .and_then(parse_u64ish),
        thread_name,
    })
}

/// Decodes events from a slice of JSON values. Returns the events and how many were skipped.
pub fn decode_trace_values(values: &[Value]) -> (Vec<TraceEvent>, u32) {
    let mut skipped = 0;
    let events = values
        .iter()
        .filter_map(|v| {
            let e = decode_event(v);
            if e.is_none() {
                skipped += 1;
            }
            e
        })
        .collect();
    (events, skipped)
}

/// Decodes trace events from JSON text: either a `getVMTimeline` reply (its `traceEvents`) or a
/// bare array of events. Unusable input counts as nothing decoded and nothing skipped.
pub fn decode_trace_events(json: &str) -> (Vec<TraceEvent>, u32) {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Doc {
        Reply {
            #[serde(rename = "traceEvents")]
            trace_events: Vec<Value>,
        },
        Array(Vec<Value>),
    }
    match serde_json::from_str::<Doc>(json) {
        Ok(Doc::Reply { trace_events }) => decode_trace_values(&trace_events),
        Ok(Doc::Array(a)) => decode_trace_values(&a),
        Err(_) => (Vec::new(), 0),
    }
}

impl VmService {
    pub async fn get_vm_timeline_flags(&self) -> Result<TimelineFlags, RpcError> {
        decode(self.client.call("getVMTimelineFlags", json!({})).await?)
    }

    /// Replaces the whole recorded set. An unknown name fails the call (see [`wanted_streams`]).
    pub async fn set_vm_timeline_flags(&self, streams: &[&str]) -> Result<(), RpcError> {
        self.client
            .call("setVMTimelineFlags", json!({"recordedStreams": streams}))
            .await
            .map(|_| ())
    }

    /// Unsubscribes from a stream. Error 104 ("Stream not subscribed") counts as success.
    pub async fn stream_cancel(&self, stream: &str) -> Result<(), RpcError> {
        match self
            .client
            .call("streamCancel", json!({"streamId": stream}))
            .await
        {
            Ok(_) | Err(RpcError::Server { code: 104, .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// JSON `getVMTimeline` for `[origin, origin + extent]`. The reply stays text until it is
    /// decoded on a blocking thread, so the reader task is never held up by a large body.
    pub async fn get_vm_timeline(
        &self,
        origin_us: i64,
        extent_us: i64,
        timeout: Duration,
    ) -> Result<TimelineChunk, RpcError> {
        let raw = self
            .client
            .call_raw_with_timeout(
                "getVMTimeline",
                json!({"timeOriginMicros": origin_us, "timeExtentMicros": extent_us}),
                timeout,
            )
            .await?;
        tokio::task::spawn_blocking(move || {
            #[derive(Deserialize)]
            struct Reply {
                #[serde(rename = "traceEvents", default)]
                trace_events: Vec<Value>,
            }
            let r: Reply = serde_json::from_str(raw.get())
                .map_err(|e| RpcError::Decode(format!("getVMTimeline: {e}")))?;
            let (events, skipped) = decode_trace_values(&r.trace_events);
            Ok(TimelineChunk { events, skipped })
        })
        .await
        .map_err(|e| RpcError::Decode(format!("getVMTimeline: {e}")))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{StreamEvent, decode_notification};
    use crate::rpc::Notification;

    fn flags(available: &[&str], recorded: &[&str]) -> TimelineFlags {
        TimelineFlags {
            available: available.iter().map(|s| s.to_string()).collect(),
            recorded: recorded.iter().map(|s| s.to_string()).collect(),
        }
    }

    const ALL: &[&str] = &[
        "API",
        "Compiler",
        "CompilerVerbose",
        "Dart",
        "Debugger",
        "Embedder",
        "GC",
        "Isolate",
        "Microtask",
        "VM",
    ];

    #[test]
    fn wanted_streams_cases() {
        assert_eq!(
            wanted_streams(&flags(ALL, &["Dart", "Embedder", "GC", "Microtask"])),
            None
        );
        assert_eq!(
            wanted_streams(&flags(ALL, &["GC"])),
            Some(vec!["GC".into(), "Dart".into(), "Embedder".into()])
        );
        let no_embedder: Vec<&str> = ALL.iter().copied().filter(|s| *s != "Embedder").collect();
        assert_eq!(
            wanted_streams(&flags(&no_embedder, &["GC"])),
            Some(vec!["GC".into(), "Dart".into()])
        );
        assert_eq!(
            wanted_streams(&flags(ALL, &["Mystery"])),
            Some(vec!["Mystery".into(), "Dart".into(), "Embedder".into()])
        );
        assert_eq!(wanted_streams(&flags(&["GC"], &["GC"])), None);
    }

    #[test]
    fn trace_event_phases_decode() {
        let json = r#"{"type":"Timeline","traceEvents":[
          {"args":{"isolateId":"isolates/1"},"cat":"Dart","name":"LAYOUT","ph":"B","pid":20931,"tid":259,"ts":420817589524},
          {"cat":"Dart","name":"LAYOUT","ph":"E","pid":20931,"tid":259,"ts":420817589600},
          {"cat":"Embedder","name":"Work","ph":"X","pid":1,"tid":2,"ts":10,"dur":5},
          {"args":{},"cat":"Embedder","id":"1fdb","name":"PipelineItem","ph":"b","pid":20931,"tid":259,"ts":420817596819},
          {"cat":"Embedder","id":"1fdb","name":"PipelineItem","ph":"e","pid":20931,"tid":259,"ts":420817596900},
          {"cat":"GC","name":"Tick","ph":"i","pid":1,"tid":2,"ts":11},
          {"cat":"Dart","name":"RSS","ph":"C","pid":1,"tid":2,"ts":12,"args":{"RSS":5}},
          {"name":"thread_name","ph":"M","pid":20931,"tid":259,"ts":0,"args":{"name":"io.flutter.platform"}},
          {"cat":"Embedder","name":"flow","ph":"s","pid":1,"tid":2,"ts":13,"id":"3"}
        ],"timeOriginMicros":1,"timeExtentMicros":2}"#;
        let (ev, skipped) = decode_trace_events(json);
        assert_eq!(skipped, 0);
        let phases: Vec<Phase> = ev.iter().map(|e| e.ph).collect();
        assert_eq!(
            phases,
            [
                Phase::Begin,
                Phase::End,
                Phase::Complete,
                Phase::AsyncBegin,
                Phase::AsyncEnd,
                Phase::Instant,
                Phase::Counter,
                Phase::Metadata,
                Phase::Other
            ]
        );
        assert_eq!(&*ev[0].name, "LAYOUT");
        assert_eq!(ev[0].cat, Category::Dart);
        assert_eq!(
            (ev[0].pid, ev[0].tid, ev[0].ts_us),
            (20931, 259, 420817589524)
        );
        assert_eq!(ev[2].dur_us, Some(5));
        assert_eq!(ev[2].cat, Category::Embedder);
        assert_eq!(ev[5].cat, Category::Gc);
        assert_eq!(ev[7].thread_name.as_deref(), Some("io.flutter.platform"));
        assert_eq!(ev[0].thread_name, None);
        assert_eq!(ev[3].id, Some(0x1fdb));
    }

    #[test]
    fn async_ids_parse_hex_decimal_and_numbers() {
        let ev = |id: Value| {
            let (e, _) = decode_trace_values(&[json!({"ph":"b","ts":1,"id":id,"name":"x"})]);
            e[0].id
        };
        assert_eq!(ev(json!("1fdb")), Some(0x1fdb));
        assert_eq!(ev(json!("0x1fdb")), Some(0x1fdb));
        assert_eq!(ev(json!(8155)), Some(8155));
        // Too long to be a u64 in hex, so it is read as decimal.
        assert_eq!(
            ev(json!("15148060492213163658")),
            Some(15148060492213163658)
        );
        assert_eq!(ev(json!("zzz")), None);
        assert_eq!(ev(Value::Null), None);
    }

    #[test]
    fn frame_number_comes_from_args() {
        let (e, _) = decode_trace_values(&[
            json!({"ph":"B","ts":1,"name":"Animator::BeginFrame","args":{"frame_number":"2778"}}),
            json!({"ph":"B","ts":2,"name":"x","args":{"frame_number":12}}),
            json!({"ph":"B","ts":3,"name":"y","args":{}}),
        ]);
        assert_eq!(e[0].frame_number, Some(2778));
        assert_eq!(e[1].frame_number, Some(12));
        assert_eq!(e[2].frame_number, None);
    }

    #[test]
    fn metadata_events_need_no_timestamp() {
        let (e, skipped) = decode_trace_events(
            r#"[{"name":"thread_name","ph":"M","pid":1,"tid":7,"args":{"name":"DartWorker (7)"}},
                {"name":"x","ph":"B","pid":1,"tid":7}]"#,
        );
        assert_eq!(skipped, 1, "only the B event without ts is skipped");
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].thread_name.as_deref(), Some("DartWorker (7)"));
        assert_eq!(e[0].ts_us, 0);
    }

    #[test]
    fn bad_events_are_skipped_and_counted() {
        let (e, skipped) = decode_trace_values(&[
            json!("nope"),
            json!({"name":"no phase","ts":1}),
            json!({"ph":"B","name":"no ts"}),
            json!({"ph":"B","name":"ok","ts":5}),
            json!({"ph":"B","name":"float ts","ts":5.7}),
        ]);
        assert_eq!(skipped, 3);
        assert_eq!(e.len(), 2);
        assert_eq!(e[1].ts_us, 5);
        assert_eq!(decode_trace_events("not json"), (vec![], 0));
        assert_eq!(decode_trace_events("[{\"ph\":\"B\",\"ts\":1}]").0.len(), 1);
    }

    fn note(event: Value) -> Notification {
        Notification {
            method: "streamNotify".into(),
            params: json!({"streamId": "Timeline", "event": event}),
        }
    }

    #[test]
    fn timeline_events_notification_decodes() {
        let ev = decode_notification(&note(json!({
            "kind":"TimelineEvents","timestamp":1,
            "timelineEvents":[{"ph":"B","ts":7,"name":"a","cat":"Dart","pid":1,"tid":2},{"bad":1}]
        })))
        .unwrap();
        match ev {
            StreamEvent::TimelineEvents { events } => {
                assert_eq!(events.len(), 1);
                assert_eq!(events[0].ts_us, 7);
            }
            other => panic!("{other:?}"),
        }
        let ev = decode_notification(&note(json!({
            "kind":"TimelineStreamSubscriptionsUpdate","timestamp":1,
            "updatedStreams":["Dart","Embedder","GC"]
        })))
        .unwrap();
        assert_eq!(
            ev,
            StreamEvent::TimelineSubscriptions {
                recorded: vec!["Dart".into(), "Embedder".into(), "GC".into()]
            }
        );
    }

    #[tokio::test]
    async fn stream_cancel_accepts_not_subscribed_and_timeline_calls_work() {
        use crate::rpc::{ConnectOptions, connect};
        use crate::testing::ScriptedServer;
        let server = ScriptedServer::start(|m, p| match m {
            "streamCancel" => Err((104, "Stream not subscribed".to_string())),
            "getVMTimelineFlags" => {
                Ok(json!({"availableStreams":["Dart"],"recordedStreams":["GC"],
                "recorderName":"Ring","type":"TimelineFlags"}))
            }
            "setVMTimelineFlags" => {
                assert_eq!(p["recordedStreams"], json!(["GC", "Dart"]));
                Ok(json!({"type":"Success"}))
            }
            "getVMTimeline" => {
                assert_eq!(p["timeOriginMicros"], 10);
                assert_eq!(p["timeExtentMicros"], 20);
                Ok(json!({"type":"Timeline","traceEvents":[{"ph":"B","ts":11,"name":"a"},7]}))
            }
            _ => Err((-32601, "nope".to_string())),
        });
        let (client, _n) = connect(&server.ws_uri(), ConnectOptions::default())
            .await
            .unwrap();
        let svc = VmService::new(client);
        svc.stream_cancel("Timeline").await.unwrap();
        let f = svc.get_vm_timeline_flags().await.unwrap();
        assert_eq!(f, flags(&["Dart"], &["GC"]));
        svc.set_vm_timeline_flags(&["GC", "Dart"]).await.unwrap();
        let c = svc
            .get_vm_timeline(10, 20, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!((c.events.len(), c.skipped), (1, 1));
    }

    #[test]
    fn timeline_notification_without_payload_is_other() {
        let ev =
            decode_notification(&note(json!({"kind":"TimelineEvents","timestamp":1}))).unwrap();
        assert!(matches!(ev, StreamEvent::Other { .. }));
    }
}
