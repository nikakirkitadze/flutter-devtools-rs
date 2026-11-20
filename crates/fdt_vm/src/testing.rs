//! Local WebSocket servers for tests. Bound to 127.0.0.1 only.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;

/// A command sent to a live connection task.
pub(crate) enum ConnCmd {
    Send(String),
    /// Drop the TCP connection without a close frame.
    Abort,
}

pub(crate) type ConnTx = mpsc::UnboundedSender<ConnCmd>;
pub(crate) type ConnFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Accept/WebSocket plumbing shared by every kind of test server. Each accepted connection is
/// upgraded, registered in `conns` (so the server can broadcast to it), and handed to `serve`
/// together with the receiving end of its command channel.
pub(crate) struct Plumbing {
    pub addr: SocketAddr,
    pub conns: Arc<Mutex<Vec<ConnTx>>>,
    accept: JoinHandle<()>,
}

impl Plumbing {
    pub fn start<F>(serve: F) -> Plumbing
    where
        F: Fn(WebSocketStream<TcpStream>, mpsc::UnboundedReceiver<ConnCmd>, ConnTx) -> ConnFuture
            + Send
            + Sync
            + 'static,
    {
        let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1");
        std_listener.set_nonblocking(true).expect("nonblocking");
        let addr = std_listener.local_addr().expect("local addr");
        let listener = TcpListener::from_std(std_listener).expect("tokio listener");
        let conns: Arc<Mutex<Vec<ConnTx>>> = Arc::default();
        let registry = conns.clone();
        let serve = Arc::new(serve);
        let accept = tokio::spawn(async move {
            loop {
                let Ok((tcp, _)) = listener.accept().await else {
                    break;
                };
                let registry = registry.clone();
                let serve = serve.clone();
                tokio::spawn(async move {
                    let Ok(ws) = tokio_tungstenite::accept_async(tcp).await else {
                        return;
                    };
                    let (tx, rx) = mpsc::unbounded_channel();
                    registry.lock().unwrap().push(tx.clone());
                    serve(ws, rx, tx).await;
                });
            }
        });
        Plumbing {
            addr,
            conns,
            accept,
        }
    }

    pub fn ws_uri(&self) -> url::Url {
        url::Url::parse(&format!("ws://{}/ws", self.addr)).expect("url")
    }

    pub fn broadcast(&self, make: impl Fn() -> ConnCmd) {
        for c in self.conns.lock().unwrap().iter() {
            let _ = c.send(make());
        }
    }
}

impl Drop for Plumbing {
    fn drop(&mut self) {
        self.accept.abort();
        self.broadcast(|| ConnCmd::Abort);
    }
}

/// Responses held back by `reverse_responses`: how many to wait for, and those collected.
type Held = (usize, Vec<(ConnTx, String)>);

type Handler = dyn Fn(&str, &Value) -> Result<Value, (i64, String)> + Send + Sync;

#[derive(Default)]
struct Shared {
    open: std::sync::atomic::AtomicUsize,
    received: Mutex<Vec<(String, Value)>>,
    latency: Mutex<Duration>,
    /// Methods that are recorded but never answered.
    silenced: Mutex<std::collections::HashSet<String>>,
    /// While `Some((n, held))`, responses are collected and released in reverse once `n` arrive.
    reverse: Mutex<Option<Held>>,
}

/// A JSON-RPC server whose answers come from a closure.
pub struct ScriptedServer {
    plumbing: Plumbing,
    shared: Arc<Shared>,
}

impl ScriptedServer {
    /// Must be called inside a tokio runtime.
    pub fn start(
        handler: impl Fn(&str, &Value) -> Result<Value, (i64, String)> + Send + Sync + 'static,
    ) -> ScriptedServer {
        let handler: Arc<Handler> = Arc::new(handler);
        let shared = Arc::new(Shared::default());
        let s = shared.clone();
        let plumbing = Plumbing::start(move |ws, rx, tx| {
            Box::pin(serve_scripted(ws, rx, tx, handler.clone(), s.clone()))
        });
        ScriptedServer { plumbing, shared }
    }

    pub fn ws_uri(&self) -> url::Url {
        self.plumbing.ws_uri()
    }

    /// Sends a JSON-RPC notification to every connected client.
    pub fn notify(&self, method: &str, params: Value) {
        let text = json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string();
        self.plumbing.broadcast(|| ConnCmd::Send(text.clone()));
    }

    /// Drops every connection without a close frame.
    pub fn close_abruptly(&self) {
        self.plumbing.broadcast(|| ConnCmd::Abort);
    }

    /// Connections currently open on the server side.
    pub fn open_connections(&self) -> usize {
        self.shared.open.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn received(&self) -> Vec<(String, Value)> {
        self.shared.received.lock().unwrap().clone()
    }

    /// Never answers `method` (the request is still recorded by [`received`](Self::received)).
    pub fn silence(&self, method: &str) {
        self.shared
            .silenced
            .lock()
            .unwrap()
            .insert(method.to_string());
    }

    /// Delays every response.
    pub fn set_latency(&self, latency: Duration) {
        *self.shared.latency.lock().unwrap() = latency;
    }

    /// Holds the next `n` responses and sends them in reverse order of arrival.
    pub fn reverse_responses(&self, n: usize) {
        *self.shared.reverse.lock().unwrap() = Some((n, Vec::new()));
    }
}

async fn serve_scripted(
    mut ws: WebSocketStream<TcpStream>,
    mut rx: mpsc::UnboundedReceiver<ConnCmd>,
    tx: ConnTx,
    handler: Arc<Handler>,
    shared: Arc<Shared>,
) {
    struct Open(Arc<Shared>);
    impl Drop for Open {
        fn drop(&mut self) {
            self.0
                .open
                .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    shared
        .open
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let _open = Open(shared.clone());
    loop {
        tokio::select! {
            incoming = ws.next() => {
                let Some(Ok(msg)) = incoming else { break };
                let Message::Text(text) = msg else { continue };
                let Ok(req) = serde_json::from_str::<Value>(text.as_str()) else { continue };
                let method = req["method"].as_str().unwrap_or("").to_string();
                let params = req.get("params").cloned().unwrap_or(Value::Null);
                shared.received.lock().unwrap().push((method.clone(), params.clone()));
                let Some(id) = req.get("id").cloned() else { continue };
                if shared.silenced.lock().unwrap().contains(&method) {
                    continue;
                }
                let reply = match handler(&method, &params) {
                    Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    Err((code, message)) => {
                        json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
                    }
                }
                .to_string();
                deliver(&tx, reply, &shared);
            }
            cmd = rx.recv() => match cmd {
                Some(ConnCmd::Send(t)) => {
                    if ws.send(Message::text(t)).await.is_err() {
                        break;
                    }
                }
                Some(ConnCmd::Abort) | None => break,
            },
        }
    }
    // Dropping `ws` closes the TCP connection without a close frame.
}

fn deliver(tx: &ConnTx, reply: String, shared: &Shared) {
    {
        let mut guard = shared.reverse.lock().unwrap();
        if let Some((n, held)) = guard.as_mut() {
            held.push((tx.clone(), reply));
            if held.len() >= *n {
                let batch = std::mem::take(held);
                *guard = None;
                for (tx, reply) in batch.into_iter().rev() {
                    let _ = tx.send(ConnCmd::Send(reply));
                }
            }
            return;
        }
    }
    let latency = *shared.latency.lock().unwrap();
    if latency.is_zero() {
        let _ = tx.send(ConnCmd::Send(reply));
    } else {
        let tx = tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(latency).await;
            let _ = tx.send(ConnCmd::Send(reply));
        });
    }
}

// ---------------------------------------------------------------------------------------------
// ReplayServer
// ---------------------------------------------------------------------------------------------

/// One recorded answer: `Ok(result)` or `Err(error object)`.
type Recorded = Result<Value, Value>;

/// The request parameter that identifies a call, for methods whose answer depends on it.
fn key_param(method: &str) -> Option<&'static str> {
    match method {
        "getObject" => Some("objectId"),
        "getHttpProfileRequest" | "ext.dart.io.getHttpProfileRequest" => Some("id"),
        "ext.jank_lab.run" => Some("action"),
        _ => None,
    }
}

/// What a polling method answers once its recorded answers run out: nothing new, never a repeat
/// of the last answer (a repeat would look like fresh data).
fn empty_when_exhausted(method: &str) -> Option<Value> {
    match method {
        "getVMTimeline" => Some(json!({
            "type": "Timeline", "traceEvents": [], "timeOriginMicros": 0, "timeExtentMicros": 0
        })),
        "getPerfettoCpuSamples" => Some(json!({
            "type": "PerfettoCpuSamples", "samplePeriod": 1000, "maxStackDepth": 128,
            "sampleCount": 0, "pid": 0, "timeOriginMicros": 0, "timeExtentMicros": 0,
            "samples": ""
        })),
        _ => None,
    }
}

#[derive(Default)]
struct Transcript {
    /// Answers per method (and per key for keyed methods), in recorded order.
    answers: HashMap<(String, Option<String>), Vec<Recorded>>,
    /// Methods with at least one recorded answer; any other method is "not found".
    methods: HashSet<String>,
    /// Recorded notifications per stream, in recorded order.
    notifications: HashMap<String, Vec<Value>>,
}

impl Transcript {
    fn load(path: &Path) -> Transcript {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|e| panic!("cannot read transcript {}: {e}", path.display()));
        let mut t = Transcript::default();
        // Requests by id, awaiting their responses.
        let mut pending: HashMap<String, (String, Option<String>)> = HashMap::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let Ok(entry) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let msg = &entry["msg"];
            match entry["dir"].as_str() {
                Some("send") => {
                    let (Some(method), Some(id)) = (msg["method"].as_str(), msg.get("id")) else {
                        continue;
                    };
                    let key = key_param(method)
                        .and_then(|k| msg["params"].get(k))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    pending.insert(id_key(id), (method.to_string(), key));
                }
                Some("recv") => {
                    if let Some(id) = msg.get("id") {
                        let Some((method, key)) = pending.remove(&id_key(id)) else {
                            continue;
                        };
                        let rec = match msg.get("error") {
                            Some(e) => Err(e.clone()),
                            None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                        };
                        t.methods.insert(method.clone());
                        t.answers.entry((method, key)).or_default().push(rec);
                    } else if msg["method"] == "streamNotify"
                        && let Some(stream) = msg["params"]["streamId"].as_str()
                    {
                        t.notifications
                            .entry(stream.to_string())
                            .or_default()
                            .push(msg.clone());
                    }
                }
                _ => {}
            }
        }
        t
    }
}

fn id_key(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

struct ReplayShared {
    transcript: Transcript,
    /// How many times each (method, key) has been answered.
    served: Mutex<HashMap<(String, Option<String>), usize>>,
    received: Mutex<Vec<(String, Value)>>,
}

impl ReplayShared {
    /// The recorded answer for this request, or the error for one nothing recorded can answer.
    fn answer(&self, method: &str, params: &Value) -> Recorded {
        if !self.transcript.methods.contains(method) {
            return Err(json!({"code": -32601, "message": "Method not found"}));
        }
        let key = key_param(method)
            .and_then(|k| params.get(k))
            .and_then(Value::as_str)
            .map(str::to_string);
        // `isolateId` and every other parameter are ignored apart from the key.
        let slot = (method.to_string(), key);
        let Some(list) = self.transcript.answers.get(&slot) else {
            return Err(json!({"code": -32602, "message": "Invalid params"}));
        };
        let mut served = self.served.lock().unwrap();
        let n = served.entry(slot).or_insert(0);
        if *n >= list.len()
            && let Some(empty) = empty_when_exhausted(method)
        {
            return Ok(empty);
        }
        let rec = list[(*n).min(list.len() - 1)].clone();
        *n += 1;
        rec
    }
}

/// Serves the responses recorded in a JSONL transcript (as written by `tap::Tap`).
///
/// Matching rules:
/// - `getObject` is matched by `objectId`, `getHttpProfileRequest` by `id` and
///   `ext.jank_lab.run` by `action`. A key that was never recorded is `-32602 "Invalid params"`.
/// - `getVMTimeline` and `getPerfettoCpuSamples` are answered with their recorded responses in
///   recorded order, then with an empty timeline or an empty sample set (never a repeat).
/// - Every other method is answered with its recorded responses in recorded order, then the
///   last one again. `isolateId` and all other parameters are ignored.
/// - A method that was never recorded is `-32601 "Method not found"`.
/// - After answering `streamListen` for stream S, every recorded notification of S is sent, in
///   recorded order.
pub struct ReplayServer {
    plumbing: Plumbing,
    shared: Arc<ReplayShared>,
}

impl ReplayServer {
    /// Must be called inside a tokio runtime. Panics if the transcript cannot be read.
    pub fn start(transcript: &Path) -> ReplayServer {
        let shared = Arc::new(ReplayShared {
            transcript: Transcript::load(transcript),
            served: Mutex::default(),
            received: Mutex::default(),
        });
        let s = shared.clone();
        let plumbing =
            Plumbing::start(move |ws, rx, tx| Box::pin(serve_replay(ws, rx, tx, s.clone())));
        ReplayServer { plumbing, shared }
    }

    pub fn ws_uri(&self) -> url::Url {
        self.plumbing.ws_uri()
    }

    pub fn received(&self) -> Vec<(String, Value)> {
        self.shared.received.lock().unwrap().clone()
    }
}

async fn serve_replay(
    mut ws: WebSocketStream<TcpStream>,
    mut rx: mpsc::UnboundedReceiver<ConnCmd>,
    tx: ConnTx,
    shared: Arc<ReplayShared>,
) {
    loop {
        tokio::select! {
            incoming = ws.next() => {
                let Some(Ok(msg)) = incoming else { break };
                let Message::Text(text) = msg else { continue };
                let Ok(req) = serde_json::from_str::<Value>(text.as_str()) else { continue };
                let method = req["method"].as_str().unwrap_or("").to_string();
                let params = req.get("params").cloned().unwrap_or(Value::Null);
                shared.received.lock().unwrap().push((method.clone(), params.clone()));
                let Some(id) = req.get("id").cloned() else { continue };
                let answer = shared.answer(&method, &params);
                let ok = answer.is_ok();
                let reply = match answer {
                    Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
                    Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
                }
                .to_string();
                let _ = tx.send(ConnCmd::Send(reply));
                if ok
                    && method == "streamListen"
                    && let Some(stream) = params["streamId"].as_str()
                    && let Some(events) = shared.transcript.notifications.get(stream)
                {
                    for event in events {
                        let _ = tx.send(ConnCmd::Send(event.to_string()));
                    }
                }
            }
            cmd = rx.recv() => match cmd {
                Some(ConnCmd::Send(t)) => {
                    if ws.send(Message::text(t)).await.is_err() {
                        break;
                    }
                }
                Some(ConnCmd::Abort) | None => break,
            },
        }
    }
}
