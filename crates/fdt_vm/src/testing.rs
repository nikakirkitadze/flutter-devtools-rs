//! Local WebSocket servers for tests. Bound to 127.0.0.1 only.

use std::future::Future;
use std::net::SocketAddr;
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
    received: Mutex<Vec<(String, Value)>>,
    latency: Mutex<Duration>,
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

    pub fn received(&self) -> Vec<(String, Value)> {
        self.shared.received.lock().unwrap().clone()
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
