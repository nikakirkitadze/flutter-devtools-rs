//! HTTP profile capture: the per-isolate poller, body fetches and the session-side bookkeeping.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fdt_store::SessionWriter;
use fdt_store::clock::RunClock;
use fdt_store::net::{BodyKind, BodyState, HttpUpdate};
use fdt_store::time::{EntryId, RunId};
use fdt_vm::dart_io::HttpProfileEntry;
use fdt_vm::rpc::RpcError;
use fdt_vm::service::VmService;
use tokio::sync::{Semaphore, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval};

use super::{Ingest, Sink, sockets};

pub(crate) const ENABLE_RPC: &str = "ext.dart.io.httpEnableTimelineLogging";
const MAX_FETCHES_PER_RUN: usize = 4;
pub(crate) const RECONNECT_REASON: &str = "Reconnect to the app to load the rest.";

/// Maps one profile entry onto the store's update. HTTP profile times are epoch microseconds.
pub(crate) fn to_update(entry: &HttpProfileEntry, clock: &RunClock, run: RunId) -> HttpUpdate {
    let time = |us: i64| clock.from_epoch_us(us).time;
    let start = clock.from_epoch_us(entry.start_time);
    let req = entry.request.as_ref();
    let resp = entry.response.as_ref();
    let error = req
        .and_then(|r| r.error.clone())
        .or_else(|| resp.and_then(|r| r.error.clone()));
    let end = resp
        .and_then(|r| r.end_time)
        .or_else(|| {
            error.as_ref()?;
            Some(
                entry
                    .end_time
                    .or_else(|| entry.events.last().map(|e| e.timestamp))
                    .or_else(|| resp.map(|r| r.start_time))
                    .unwrap_or(entry.start_time),
            )
        })
        .map(time);
    let headers = |h: Option<&fdt_vm::dart_io::Headers>| {
        // A BTreeMap iterates in name order.
        h.map(|h| h.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    };
    let remote = req
        .and_then(|r| r.connection_info.as_ref())
        .or_else(|| resp.and_then(|r| r.connection_info.as_ref()))
        .and_then(|c| {
            Some((
                c.remote_address.clone()?,
                u16::try_from(c.remote_port?).ok()?,
            ))
        });
    HttpUpdate {
        run,
        isolate: entry.isolate_id.clone(),
        vm_id: entry.id.clone(),
        method: entry.method.clone(),
        url: entry.uri.clone(),
        start: start.time,
        start_clamped: start.clamped,
        request_sent: entry.end_time.map(time),
        response_start: resp.map(|r| time(r.start_time)),
        end,
        status: resp.and_then(|r| r.status_code),
        reason: resp.and_then(|r| r.reason_phrase.clone()),
        request_headers: headers(req.and_then(|r| r.headers.as_ref())),
        response_headers: headers(resp.and_then(|r| r.headers.as_ref())),
        request_content_length: req.and_then(|r| r.content_length),
        response_content_length: resp.and_then(|r| r.content_length),
        events: entry
            .events
            .iter()
            .map(|e| (e.event.clone(), time(e.timestamp)))
            .collect(),
        error,
        websocket: entry.is_websocket_upgrade(),
        remote,
    }
}

/// What the session tells a poller after storing its entries.
pub(crate) struct PollReply {
    /// Newly completed requests whose bodies should be fetched: `(id, vm_id)`.
    pub(crate) fetch: Vec<(EntryId, String)>,
    /// Some request of this isolate is still incomplete in the store.
    pub(crate) incomplete: bool,
    /// This isolate has made a websocket request.
    pub(crate) has_websocket: bool,
}

pub(crate) enum Fetched {
    Auto {
        request: Option<Vec<u8>>,
        response: Option<Vec<u8>>,
    },
    Full {
        kind: BodyKind,
        bytes: Option<Vec<u8>>,
    },
    Failed {
        kinds: Vec<BodyKind>,
        reason: String,
    },
}

#[derive(Clone, Copy)]
pub(crate) enum FetchMode {
    Auto,
    Full(BodyKind),
}

/// Fetches request and response bodies, at most [`MAX_FETCHES_PER_RUN`] at a time. Each fetch is
/// its own task, so polling and event ingest never wait for one.
pub(crate) struct BodyFetcher {
    svc: VmService,
    sink: Sink,
    permits: Semaphore,
    in_flight: Mutex<HashMap<String, usize>>,
}

struct Counted<'a>(&'a BodyFetcher, String);

impl Drop for Counted<'_> {
    fn drop(&mut self) {
        let mut m = self.0.in_flight.lock().unwrap();
        if let Some(n) = m.get_mut(&self.1) {
            *n = n.saturating_sub(1);
        }
    }
}

impl BodyFetcher {
    pub(crate) fn new(svc: VmService, sink: Sink) -> BodyFetcher {
        BodyFetcher {
            svc,
            sink,
            permits: Semaphore::new(MAX_FETCHES_PER_RUN),
            in_flight: Mutex::default(),
        }
    }

    /// Fetches in `isolate` that have not finished.
    pub(crate) fn in_flight(&self, isolate: &str) -> usize {
        self.in_flight
            .lock()
            .unwrap()
            .get(isolate)
            .copied()
            .unwrap_or(0)
    }

    pub(crate) fn spawn(
        self: &Arc<Self>,
        isolate: String,
        id: EntryId,
        vm_id: String,
        mode: FetchMode,
    ) {
        // Counted before the task exists, so a poller never sees a gap.
        *self
            .in_flight
            .lock()
            .unwrap()
            .entry(isolate.clone())
            .or_insert(0) += 1;
        let me = self.clone();
        tokio::spawn(async move {
            let _count = Counted(&me, isolate.clone());
            let result = me.fetch(&isolate, &vm_id, mode).await;
            // Sent before the count drops, so the session sees the body before any later poll.
            me.sink.send(Ingest::Body { id, result });
        });
    }

    async fn fetch(&self, isolate: &str, vm_id: &str, mode: FetchMode) -> Fetched {
        let kinds = match mode {
            FetchMode::Auto => vec![BodyKind::Request, BodyKind::Response],
            FetchMode::Full(k) => vec![k],
        };
        let failed = |e: RpcError| Fetched::Failed {
            kinds: kinds.clone(),
            reason: e.to_string(),
        };
        let _permit = match self.permits.acquire().await {
            Ok(p) => p,
            Err(_) => return failed(RpcError::Closed),
        };
        match self.svc.get_http_profile_request(isolate, vm_id).await {
            Err(e) => failed(e),
            Ok(full) => match mode {
                FetchMode::Auto => Fetched::Auto {
                    request: full.request_body,
                    response: full.response_body,
                },
                FetchMode::Full(kind) => Fetched::Full {
                    kind,
                    bytes: match kind {
                        BodyKind::Request => full.request_body,
                        BodyKind::Response => full.response_body,
                    },
                },
            },
        }
    }
}

/// Everything a poller needs.
pub(crate) struct PollerConfig {
    pub(crate) http_poll: Duration,
    pub(crate) socket_poll: Duration,
    /// Shared with the session, which flips it live.
    pub(crate) free_app_memory: Arc<AtomicBool>,
}

/// One poller task per isolate that exposes the HTTP profile.
pub(crate) struct NetHub {
    sink: Sink,
    svc: VmService,
    fetcher: Arc<BodyFetcher>,
    cfg: Arc<PollerConfig>,
    pollers: HashMap<String, JoinHandle<()>>,
}

impl NetHub {
    pub(crate) fn new(
        sink: Sink,
        svc: VmService,
        fetcher: Arc<BodyFetcher>,
        cfg: PollerConfig,
    ) -> NetHub {
        NetHub {
            sink,
            svc,
            fetcher,
            cfg: Arc::new(cfg),
            pollers: HashMap::new(),
        }
    }

    /// Enables profiling in `isolate` and polls it, unless that is already going.
    pub(crate) fn start(&mut self, isolate: &str) {
        if self.pollers.get(isolate).is_some_and(|h| !h.is_finished()) {
            return;
        }
        let task = poll_isolate(
            self.sink.clone(),
            self.svc.clone(),
            self.fetcher.clone(),
            self.cfg.clone(),
            isolate.to_string(),
        );
        self.pollers.insert(isolate.to_string(), tokio::spawn(task));
    }

    pub(crate) fn stop(&mut self, isolate: &str) {
        if let Some(h) = self.pollers.remove(isolate) {
            h.abort();
        }
    }
}

impl Drop for NetHub {
    fn drop(&mut self) {
        for h in self.pollers.values() {
            h.abort();
        }
    }
}

async fn poll_isolate(
    sink: Sink,
    svc: VmService,
    fetcher: Arc<BodyFetcher>,
    cfg: Arc<PollerConfig>,
    isolate: String,
) {
    match svc.http_enable(&isolate, true).await {
        Err(RpcError::Closed) => return,
        Err(e) => log::warn!("enabling HTTP profiling in {isolate} failed: {e}"),
        Ok(_) => {}
    }
    match svc.socket_profiling_enable(&isolate, true).await {
        Err(RpcError::Closed) => return,
        Err(e) => log::warn!("enabling socket profiling in {isolate} failed: {e}"),
        Ok(()) => {}
    }
    let mut http = interval(cfg.http_poll);
    let mut sock = interval(cfg.socket_poll);
    for t in [&mut http, &mut sock] {
        t.set_missed_tick_behavior(MissedTickBehavior::Delay);
    }
    // The first poll asks for everything: another tool may have enabled profiling before us.
    let mut since: Option<i64> = None;
    let mut has_websocket = false;
    loop {
        tokio::select! {
            _ = http.tick() => {
                let profile = match svc.get_http_profile(&isolate, since).await {
                    Ok(p) => p,
                    Err(RpcError::Closed) => return,
                    Err(e) => {
                        log::debug!("getHttpProfile failed: {e}");
                        continue;
                    }
                };
                let updates = profile
                    .requests
                    .iter()
                    .map(|e| {
                        let mut u = to_update(e, &sink.clock, sink.run);
                        u.isolate = isolate.clone();
                        u
                    })
                    .collect();
                let (reply, rx) = oneshot::channel();
                sink.send(Ingest::HttpPoll { isolate: isolate.clone(), updates, reply });
                let Ok(reply) = rx.await else { return };
                since = Some(profile.timestamp);
                has_websocket = reply.has_websocket;
                for (id, vm_id) in reply.fetch {
                    fetcher.spawn(isolate.clone(), id, vm_id, FetchMode::Auto);
                }
                if cfg.free_app_memory.load(Ordering::Relaxed)
                    && !reply.incomplete
                    && fetcher.in_flight(&isolate) == 0
                {
                    match svc.clear_http_profile(&isolate).await {
                        Ok(()) => since = None,
                        Err(RpcError::Closed) => return,
                        Err(e) => log::debug!("clearHttpProfile failed: {e}"),
                    }
                }
            }
            _ = sock.tick() => {
                if has_websocket && !sockets::poll(&sink, &svc, &isolate).await {
                    return;
                }
            }
        }
    }
}

/// The session's view of HTTP capture. Only the session task touches it.
#[derive(Default)]
pub(crate) struct NetState {
    fetcher: Option<Arc<BodyFetcher>>,
    /// Where each request came from, for body loads.
    keys: HashMap<EntryId, (String, String)>,
    incomplete: HashMap<String, HashSet<String>>,
    websocket_isolates: HashSet<String>,
}

impl NetState {
    pub(crate) fn set_fetcher(&mut self, f: Option<Arc<BodyFetcher>>) {
        self.fetcher = f;
    }

    /// A new run starts with nothing in flight.
    pub(crate) fn new_run(&mut self) {
        self.incomplete.clear();
        self.websocket_isolates.clear();
    }

    pub(crate) fn poll(
        &mut self,
        w: &mut SessionWriter,
        auto_fetch_limit: u64,
        isolate: &str,
        updates: Vec<HttpUpdate>,
    ) -> PollReply {
        let mut fetch = Vec::new();
        for u in updates {
            let (vm_id, complete, websocket) = (u.vm_id.clone(), u.end.is_some(), u.websocket);
            let len = u.response_content_length;
            let up = w.upsert_request(u);
            self.keys
                .entry(up.id)
                .or_insert_with(|| (isolate.to_string(), vm_id.clone()));
            let open = self.incomplete.entry(isolate.to_string()).or_default();
            if complete {
                open.remove(&vm_id);
            } else {
                open.insert(vm_id.clone());
            }
            if websocket {
                self.websocket_isolates.insert(isolate.to_string());
            }
            if !up.newly_completed || websocket {
                continue;
            }
            match len {
                Some(n) if n > 0 && n as u64 > auto_fetch_limit => {
                    w.set_body_state(
                        up.id,
                        BodyKind::Response,
                        BodyState::TooLarge { len: n as u64 },
                    );
                }
                _ => {
                    w.set_body_state(up.id, BodyKind::Request, BodyState::Pending);
                    w.set_body_state(up.id, BodyKind::Response, BodyState::Pending);
                    fetch.push((up.id, vm_id));
                }
            }
        }
        PollReply {
            fetch,
            incomplete: self.incomplete.get(isolate).is_some_and(|s| !s.is_empty()),
            has_websocket: self.websocket_isolates.contains(isolate),
        }
    }

    pub(crate) fn body(&mut self, w: &mut SessionWriter, id: EntryId, result: Fetched) {
        let store = |w: &mut SessionWriter, kind, bytes: Option<Vec<u8>>, full: bool| match bytes {
            Some(b) if !b.is_empty() && full => w.store_full_body(id, kind, b),
            Some(b) if !b.is_empty() => w.store_body(id, kind, b),
            _ => w.set_body_state(id, kind, BodyState::NotCaptured),
        };
        match result {
            Fetched::Auto { request, response } => {
                store(w, BodyKind::Request, request, false);
                store(w, BodyKind::Response, response, false);
            }
            Fetched::Full { kind, bytes } => store(w, kind, bytes, true),
            Fetched::Failed { kinds, reason } => {
                for kind in kinds {
                    w.set_body_state(
                        id,
                        kind,
                        BodyState::Unavailable {
                            reason: reason.clone(),
                        },
                    );
                }
            }
        }
    }

    /// Fetches a whole body again. Only possible while connected to the request's run.
    pub(crate) fn load_full(
        &mut self,
        w: &mut SessionWriter,
        id: EntryId,
        kind: BodyKind,
        connected_run: Option<RunId>,
    ) {
        let Some((isolate, vm_id)) = self.keys.get(&id).cloned() else {
            return;
        };
        match &self.fetcher {
            Some(f) if connected_run == Some(id.run) => {
                w.set_body_state(id, kind, BodyState::Pending);
                f.spawn(isolate, id, vm_id, FetchMode::Full(kind));
            }
            _ => w.set_body_state(
                id,
                kind,
                BodyState::Unavailable {
                    reason: RECONNECT_REASON.to_string(),
                },
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fdt_store::time::SessionTime;
    use serde_json::json;

    const US: u64 = 1000;

    fn clock() -> RunClock {
        RunClock {
            origin_host_us: 0,
            skew_us: 0,
            timeline_to_wall_us: 0,
        }
    }

    fn entry(v: serde_json::Value) -> HttpProfileEntry {
        serde_json::from_value(v).unwrap()
    }

    fn convert(v: serde_json::Value) -> HttpUpdate {
        to_update(&entry(v), &clock(), RunId(1))
    }

    fn completed_get() -> serde_json::Value {
        json!({"id": "-889922867296914988", "isolateId": "isolates/9", "method": "GET",
            "uri": "http://127.0.0.1:64583/v2/products?page=3",
            "startTime": 1_000_000, "endTime": 1_000_100,
            "events": [{"event": "Request sent", "timestamp": 1_000_100},
                       {"event": "Content Download", "timestamp": 1_000_900}],
            "request": {"headers": {"user-agent": ["Dart"], "accept-encoding": ["gzip"]},
                "connectionInfo": {"localPort": 64636, "remoteAddress": "127.0.0.1", "remotePort": 64583},
                "contentLength": 0},
            "response": {"startTime": 1_000_500, "endTime": 1_001_000, "statusCode": 200,
                "reasonPhrase": "OK", "headers": {"content-type": ["application/json"]},
                "contentLength": 1039}})
    }

    #[test]
    fn a_completed_get_maps_every_field() {
        let u = convert(completed_get());
        assert_eq!(u.run, RunId(1));
        assert_eq!(u.isolate, "isolates/9");
        assert_eq!(u.vm_id, "-889922867296914988");
        assert_eq!(u.method, "GET");
        assert_eq!(u.url, "http://127.0.0.1:64583/v2/products?page=3");
        assert_eq!(u.start, SessionTime(1_000_000 * US));
        assert!(!u.start_clamped);
        assert_eq!(u.request_sent, Some(SessionTime(1_000_100 * US)));
        assert_eq!(u.response_start, Some(SessionTime(1_000_500 * US)));
        assert_eq!(u.end, Some(SessionTime(1_001_000 * US)));
        assert_eq!(u.status, Some(200));
        assert_eq!(u.reason.as_deref(), Some("OK"));
        let names: Vec<_> = u.request_headers.iter().map(|h| h.0.as_str()).collect();
        assert_eq!(names, ["accept-encoding", "user-agent"]);
        assert_eq!(u.response_content_length, Some(1039));
        assert_eq!(u.events.len(), 2);
        assert_eq!(
            u.events[0],
            ("Request sent".to_string(), SessionTime(1_000_100 * US))
        );
        assert_eq!(u.error, None);
        assert!(!u.websocket);
        assert_eq!(u.remote, Some(("127.0.0.1".to_string(), 64583)));
    }

    #[test]
    fn a_failed_request_ends_at_the_entry_end_time() {
        let u = convert(
            json!({"id": "5", "isolateId": "isolates/9", "method": "GET",
            "uri": "http://127.0.0.1:1/", "startTime": 2_000_000, "endTime": 2_000_300,
            "events": [{"event": "Connection established", "timestamp": 2_000_200}],
            "request": {"error": "SocketException: Connection refused"}}),
        );
        assert_eq!(u.end, Some(SessionTime(2_000_300 * US)));
        assert!(u.error.as_deref().unwrap().starts_with("SocketException"));
        assert_eq!(u.status, None);
        assert_eq!(u.remote, None);
    }

    #[test]
    fn a_failed_request_without_an_end_time_falls_back() {
        // The last event, then the response start, then the request start.
        let base = |events: serde_json::Value, response: serde_json::Value| {
            let mut v = json!({"id": "5", "method": "GET", "uri": "http://h/", "startTime": 100,
                "events": events, "request": {"error": "boom"}});
            if !response.is_null() {
                v["response"] = response;
            }
            convert(v).end
        };
        assert_eq!(
            base(
                json!([{"event": "a", "timestamp": 140}, {"event": "b", "timestamp": 160}]),
                json!(null)
            ),
            Some(SessionTime(160 * US))
        );
        assert_eq!(
            base(json!([]), json!({"startTime": 150, "error": "x"})),
            Some(SessionTime(150 * US))
        );
        assert_eq!(base(json!([]), json!(null)), Some(SessionTime(100 * US)));
    }

    #[test]
    fn an_in_flight_request_has_no_end_and_no_status() {
        let u = convert(
            json!({"id": "6", "isolateId": "isolates/9", "method": "GET",
            "uri": "http://127.0.0.1:64583/slow", "startTime": 3_000_000, "endTime": 3_000_050,
            "events": [{"event": "Request sent", "timestamp": 3_000_050}],
            "request": {"headers": {}, "contentLength": 0}}),
        );
        assert_eq!(u.end, None);
        assert_eq!(u.status, None);
        assert_eq!(u.response_start, None);
        assert_eq!(u.request_sent, Some(SessionTime(3_000_050 * US)));
    }

    #[test]
    fn a_websocket_upgrade_is_complete_and_flagged() {
        let u = convert(
            json!({"id": "7", "isolateId": "isolates/9", "method": "GET",
            "uri": "http://127.0.0.1:64583/ws", "startTime": 4_000_000, "endTime": 4_000_100,
            "events": [],
            "request": {"headers": {"upgrade": ["websocket"]},
                "connectionInfo": {"localPort": 1, "remoteAddress": "127.0.0.1", "remotePort": 64583}},
            "response": {"startTime": 4_000_200, "statusCode": 101,
                "reasonPhrase": "Switching Protocols", "headers": {},
                "error": "Socket has been detached"}}),
        );
        assert!(u.websocket);
        assert_eq!(u.status, Some(101));
        assert!(u.end.is_some(), "the handshake is complete");
        assert_eq!(u.error.as_deref(), Some("Socket has been detached"));
    }

    #[test]
    fn times_before_the_origin_are_clamped() {
        let mut c = clock();
        c.origin_host_us = 5_000_000;
        let u = to_update(&entry(completed_get()), &c, RunId(1));
        assert!(u.start_clamped);
        assert_eq!(u.start, SessionTime(0));
    }
}
