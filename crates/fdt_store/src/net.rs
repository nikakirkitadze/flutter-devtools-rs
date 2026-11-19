//! HTTP request and socket records. Requests are `Arc`-shared records in
//! chunked storage, updated in place as the VM profile changes; response and
//! request bodies live under a byte budget with oldest-first eviction.

use crate::chunked::{Chunked, ChunkedSnapshot};
use crate::time::{EntryId, RunId, SessionTime};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;

/// Requests (and sockets) per chunk.
pub const NET_CHUNK: usize = 1024;
/// A websocket request and its socket must start within this of each other.
const LINK_WINDOW_NS: u64 = 2_000_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StatusClass {
    Informational,
    Success,
    Redirect,
    ClientError,
    ServerError,
    Failed,
    Pending,
}

impl StatusClass {
    pub const ALL: [StatusClass; 7] = [
        StatusClass::Informational,
        StatusClass::Success,
        StatusClass::Redirect,
        StatusClass::ClientError,
        StatusClass::ServerError,
        StatusClass::Failed,
        StatusClass::Pending,
    ];

    pub fn index(self) -> usize {
        self as usize
    }

    pub fn label(self) -> &'static str {
        match self {
            StatusClass::Informational => "1xx",
            StatusClass::Success => "2xx",
            StatusClass::Redirect => "3xx",
            StatusClass::ClientError => "4xx",
            StatusClass::ServerError => "5xx",
            StatusClass::Failed => "Failed",
            StatusClass::Pending => "Pending",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BodyKind {
    Request,
    Response,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Body {
    pub bytes: Arc<[u8]>,
    /// The body's length before any truncation.
    pub full_len: u64,
}

impl Body {
    pub fn truncated(&self) -> bool {
        (self.bytes.len() as u64) < self.full_len
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum BodyState {
    #[default]
    NotCaptured,
    Pending,
    Captured(Body),
    TooLarge {
        len: u64,
    },
    Evicted {
        len: u64,
    },
    Unavailable {
        reason: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BodyLimits {
    pub per_body: usize,
    pub total: usize,
}

impl Default for BodyLimits {
    fn default() -> Self {
        Self {
            per_body: 1 << 20,
            total: 200 << 20,
        }
    }
}

type Headers = Vec<(String, Vec<String>)>;

/// What the capture knows about a request at one poll.
#[derive(Clone, Debug)]
pub struct HttpUpdate {
    pub run: RunId,
    pub isolate: String,
    pub vm_id: String,
    pub method: String,
    pub url: String,
    pub start: SessionTime,
    pub start_clamped: bool,
    pub request_sent: Option<SessionTime>,
    pub response_start: Option<SessionTime>,
    /// Set when the request is complete.
    pub end: Option<SessionTime>,
    pub status: Option<u16>,
    pub reason: Option<String>,
    pub request_headers: Headers,
    pub response_headers: Headers,
    pub request_content_length: Option<i64>,
    pub response_content_length: Option<i64>,
    pub events: Vec<(String, SessionTime)>,
    pub error: Option<String>,
    pub websocket: bool,
    pub remote: Option<(String, u16)>,
}

#[derive(Clone, Debug)]
pub struct HttpRecord {
    pub id: EntryId,
    pub isolate: String,
    pub vm_id: String,
    pub method: String,
    pub url: String,
    /// Authority with the port.
    pub host: String,
    /// Path plus query.
    pub path: String,
    pub start: SessionTime,
    pub start_clamped: bool,
    pub request_sent: Option<SessionTime>,
    pub response_start: Option<SessionTime>,
    pub end: Option<SessionTime>,
    pub status: Option<u16>,
    pub reason: Option<String>,
    pub request_headers: Headers,
    pub response_headers: Headers,
    pub request_content_length: Option<i64>,
    pub response_content_length: Option<i64>,
    pub events: Vec<(String, SessionTime)>,
    pub error: Option<String>,
    pub websocket: bool,
    pub remote: Option<(String, u16)>,
    pub request_body: BodyState,
    pub response_body: BodyState,
    /// Started before capture began (its start time was clamped).
    pub before_capture: bool,
}

impl HttpRecord {
    fn from_update(id: EntryId, u: HttpUpdate) -> Self {
        let (host, path) = host_and_path_split(&u.url);
        Self {
            id,
            isolate: u.isolate,
            vm_id: u.vm_id,
            method: u.method,
            url: u.url,
            host,
            path,
            start: u.start,
            start_clamped: u.start_clamped,
            request_sent: u.request_sent,
            response_start: u.response_start,
            end: u.end,
            status: u.status,
            reason: u.reason,
            request_headers: u.request_headers,
            response_headers: u.response_headers,
            request_content_length: u.request_content_length,
            response_content_length: u.response_content_length,
            events: u.events,
            error: u.error,
            websocket: u.websocket,
            remote: u.remote,
            request_body: BodyState::NotCaptured,
            response_body: BodyState::NotCaptured,
            before_capture: u.start_clamped,
        }
    }

    pub fn status_class(&self) -> StatusClass {
        match self.status {
            None if self.error.is_some() => StatusClass::Failed,
            None => StatusClass::Pending,
            Some(100..=199) => StatusClass::Informational,
            Some(200..=299) => StatusClass::Success,
            Some(300..=399) => StatusClass::Redirect,
            Some(400..=499) => StatusClass::ClientError,
            Some(500..=599) => StatusClass::ServerError,
            Some(_) => StatusClass::Failed,
        }
    }

    /// Failed, 4xx or 5xx; a websocket upgrade is never an error.
    pub fn is_error(&self) -> bool {
        !self.websocket
            && matches!(
                self.status_class(),
                StatusClass::Failed | StatusClass::ClientError | StatusClass::ServerError
            )
    }

    pub fn duration_ns(&self) -> Option<u64> {
        let end = self.end?;
        (end >= self.start).then(|| end.0 - self.start.0)
    }

    /// The response content length, else the known full length of a body.
    pub fn size(&self) -> Option<u64> {
        if let Some(n) = self.response_content_length
            && n >= 0
        {
            return Some(n as u64);
        }
        match &self.response_body {
            BodyState::Captured(b) => Some(b.full_len),
            BodyState::TooLarge { len } | BodyState::Evicted { len } => Some(*len),
            _ => None,
        }
    }

    pub fn content_type(&self) -> Option<&str> {
        self.response_headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
            .and_then(|(_, v)| v.first())
            .map(String::as_str)
    }

    pub fn body(&self, kind: BodyKind) -> &BodyState {
        match kind {
            BodyKind::Request => &self.request_body,
            BodyKind::Response => &self.response_body,
        }
    }

    fn body_mut(&mut self, kind: BodyKind) -> &mut BodyState {
        match kind {
            BodyKind::Request => &mut self.request_body,
            BodyKind::Response => &mut self.response_body,
        }
    }
}

/// Splits a URL into its authority (lowercased, without credentials) and its
/// path plus query. Text that is not a URL gives host `""` and path = input.
pub fn host_and_path_split(url: &str) -> (String, String) {
    let Some((scheme, rest)) = url.split_once("://") else {
        return (String::new(), url.to_string());
    };
    if scheme.is_empty()
        || !scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
    {
        return (String::new(), url.to_string());
    }
    let rest = rest.split('#').next().unwrap_or("");
    let at = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(at);
    let host = authority
        .rsplit('@')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let path = if tail.is_empty() { "/" } else { tail };
    (host, path.to_string())
}

pub struct RequestUpsert {
    pub id: EntryId,
    pub newly_completed: bool,
}

#[derive(Clone, Debug)]
pub struct SocketUpdate {
    pub run: RunId,
    pub isolate: String,
    pub vm_id: String,
    pub address: String,
    pub port: u16,
    pub kind: String,
    pub start: SessionTime,
    pub end: Option<SessionTime>,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub last_read: Option<SessionTime>,
    pub last_write: Option<SessionTime>,
}

#[derive(Clone, Debug)]
pub struct SocketRecord {
    pub id: EntryId,
    pub isolate: String,
    pub vm_id: String,
    pub address: String,
    pub port: u16,
    pub kind: String,
    pub start: SessionTime,
    pub end: Option<SessionTime>,
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub last_read: Option<SessionTime>,
    pub last_write: Option<SessionTime>,
    /// The websocket handshake request this socket carries.
    pub websocket: Option<EntryId>,
}

type Key = (RunId, String, String);

pub(crate) struct NetStore {
    requests: Chunked<Arc<HttpRecord>>,
    sockets: Chunked<SocketRecord>,
    req_index: HashMap<Key, usize>,
    sock_index: HashMap<Key, usize>,
    sock_by_addr: HashMap<(String, u16), Vec<usize>>,
    ws_requests: Vec<usize>,
    req_socket: HashMap<usize, usize>,
    hosts: Vec<String>,
    host_set: HashSet<String>,
    hosts_cache: Arc<[String]>,
    hosts_changed: bool,
    limits: BodyLimits,
    bodies: VecDeque<(EntryId, BodyKind, usize)>,
    body_bytes: usize,
}

impl NetStore {
    pub(crate) fn new(chunk: usize) -> Self {
        Self {
            requests: Chunked::with_chunk(chunk),
            sockets: Chunked::with_chunk(chunk),
            req_index: HashMap::new(),
            sock_index: HashMap::new(),
            sock_by_addr: HashMap::new(),
            ws_requests: Vec::new(),
            req_socket: HashMap::new(),
            hosts: Vec::new(),
            host_set: HashSet::new(),
            hosts_cache: Arc::from(Vec::new()),
            hosts_changed: false,
            limits: BodyLimits::default(),
            bodies: VecDeque::new(),
            body_bytes: 0,
        }
    }

    pub(crate) fn upsert_request(&mut self, u: HttpUpdate) -> RequestUpsert {
        let key = (u.run, u.isolate.clone(), u.vm_id.clone());
        let (idx, newly_completed) = match self.req_index.get(&key).copied() {
            Some(idx) => {
                let slot = self.requests.get_mut(idx).expect("indexed request exists");
                let old = &**slot;
                let mut new = HttpRecord::from_update(old.id, u);
                new.request_body = old.request_body.clone();
                new.response_body = old.response_body.clone();
                let done = old.end.is_none() && new.end.is_some();
                *slot = Arc::new(new);
                (idx, done)
            }
            None => {
                let idx = self.requests.len();
                let id = EntryId {
                    run: u.run,
                    seq: idx as u64,
                };
                let rec = HttpRecord::from_update(id, u);
                let done = rec.end.is_some();
                self.requests.push(Arc::new(rec));
                self.req_index.insert(key, idx);
                (idx, done)
            }
        };
        let rec = self.requests.get(idx).expect("just stored");
        if self.host_set.insert(rec.host.clone()) {
            self.hosts.push(rec.host.clone());
            self.hosts_changed = true;
        }
        let (id, ws) = (rec.id, rec.websocket && rec.remote.is_some());
        if ws {
            if !self.ws_requests.contains(&idx) {
                self.ws_requests.push(idx);
            }
            self.link_request(idx);
        }
        RequestUpsert {
            id,
            newly_completed,
        }
    }

    pub(crate) fn upsert_socket(&mut self, u: SocketUpdate) -> EntryId {
        let key = (u.run, u.isolate.clone(), u.vm_id.clone());
        let idx = match self.sock_index.get(&key).copied() {
            Some(idx) => {
                let s = self.sockets.get_mut(idx).expect("indexed socket exists");
                s.kind = u.kind;
                s.start = u.start;
                s.end = u.end;
                s.read_bytes = u.read_bytes;
                s.write_bytes = u.write_bytes;
                s.last_read = u.last_read;
                s.last_write = u.last_write;
                idx
            }
            None => {
                let idx = self.sockets.len();
                let rec = SocketRecord {
                    id: EntryId {
                        run: u.run,
                        seq: idx as u64,
                    },
                    isolate: u.isolate,
                    vm_id: u.vm_id,
                    address: u.address,
                    port: u.port,
                    kind: u.kind,
                    start: u.start,
                    end: u.end,
                    read_bytes: u.read_bytes,
                    write_bytes: u.write_bytes,
                    last_read: u.last_read,
                    last_write: u.last_write,
                    websocket: None,
                };
                self.sock_by_addr
                    .entry((rec.address.clone(), rec.port))
                    .or_default()
                    .push(idx);
                self.sockets.push(rec);
                self.sock_index.insert(key, idx);
                idx
            }
        };
        self.link_socket(idx);
        EntryId {
            run: self.sockets.get(idx).expect("just stored").id.run,
            seq: idx as u64,
        }
    }

    fn gap(a: SessionTime, b: SessionTime) -> u64 {
        a.0.abs_diff(b.0)
    }

    /// Links the nearest qualifying sockets to this websocket request.
    fn link_request(&mut self, ri: usize) {
        let req = self.requests.get(ri).expect("request exists");
        let Some(remote) = req.remote.clone() else {
            return;
        };
        let start = req.start;
        let mut cands: Vec<(u64, usize)> = self
            .sock_by_addr
            .get(&remote)
            .into_iter()
            .flatten()
            .filter_map(|&si| {
                let g = Self::gap(self.sockets.get(si)?.start, start);
                (g <= LINK_WINDOW_NS).then_some((g, si))
            })
            .collect();
        cands.sort();
        for (_, si) in cands {
            self.try_link(ri, si);
        }
    }

    /// Links this socket to the nearest qualifying websocket request.
    fn link_socket(&mut self, si: usize) {
        let s = self.sockets.get(si).expect("socket exists");
        let (key, start) = ((s.address.clone(), s.port), s.start);
        let mut cands: Vec<(u64, usize)> = self
            .ws_requests
            .iter()
            .filter_map(|&ri| {
                let r = self.requests.get(ri)?;
                let (a, p) = r.remote.as_ref()?;
                ((a.as_str(), *p) == (key.0.as_str(), key.1))
                    .then(|| (Self::gap(r.start, start), ri))
            })
            .filter(|(g, _)| *g <= LINK_WINDOW_NS)
            .collect();
        cands.sort();
        for (_, ri) in cands {
            self.try_link(ri, si);
        }
    }

    /// Links when the pair qualifies and is nearer than any existing link.
    fn try_link(&mut self, ri: usize, si: usize) {
        let (Some(r), Some(s)) = (self.requests.get(ri), self.sockets.get(si)) else {
            return;
        };
        let Some((addr, port)) = &r.remote else {
            return;
        };
        let g = Self::gap(r.start, s.start);
        if *addr != s.address || *port != s.port || g > LINK_WINDOW_NS {
            return;
        }
        if s.websocket.is_some_and(|w| w.seq as usize != ri) {
            return;
        }
        let rid = r.id;
        let rstart = r.start;
        if let Some(&cur) = self.req_socket.get(&ri) {
            if cur == si {
                return;
            }
            let cur_gap = Self::gap(rstart, self.sockets.get(cur).expect("linked").start);
            if cur_gap <= g {
                return;
            }
            self.sockets.get_mut(cur).expect("linked").websocket = None;
        }
        self.sockets.get_mut(si).expect("socket exists").websocket = Some(rid);
        self.req_socket.insert(ri, si);
    }

    pub(crate) fn set_limits(&mut self, limits: BodyLimits) {
        self.limits = limits;
        self.evict();
    }

    pub(crate) fn set_body_state(&mut self, id: EntryId, kind: BodyKind, state: BodyState) {
        let idx = id.seq as usize;
        if self.requests.get(idx).is_none_or(|r| r.id != id) {
            return;
        }
        self.forget(id, kind);
        let mut state = state;
        if let BodyState::Captured(b) = &state
            && b.bytes.len() > self.limits.total
        {
            state = BodyState::TooLarge { len: b.full_len };
        }
        let accounted = match &state {
            BodyState::Captured(b) => Some(b.bytes.len()),
            _ => None,
        };
        let slot = self.requests.get_mut(idx).expect("checked above");
        *Arc::make_mut(slot).body_mut(kind) = state;
        if let Some(n) = accounted {
            self.bodies.push_back((id, kind, n));
            self.body_bytes += n;
            self.evict();
        }
    }

    pub(crate) fn store_body(&mut self, id: EntryId, kind: BodyKind, mut bytes: Vec<u8>) {
        let full_len = bytes.len() as u64;
        bytes.truncate(self.limits.per_body);
        let body = Body {
            bytes: bytes.into(),
            full_len,
        };
        self.set_body_state(id, kind, BodyState::Captured(body));
    }

    /// Drops a stored body's share of the budget.
    fn forget(&mut self, id: EntryId, kind: BodyKind) {
        if let Some(p) = self.bodies.iter().position(|e| e.0 == id && e.1 == kind) {
            let (_, _, n) = self.bodies.remove(p).expect("position is valid");
            self.body_bytes -= n;
        }
    }

    fn evict(&mut self) {
        while self.body_bytes > self.limits.total {
            let Some((id, kind, n)) = self.bodies.pop_front() else {
                break;
            };
            self.body_bytes -= n;
            let slot = self.requests.get_mut(id.seq as usize).expect("stored body");
            let rec = Arc::make_mut(slot);
            if let BodyState::Captured(b) = rec.body(kind) {
                let len = b.full_len;
                *rec.body_mut(kind) = BodyState::Evicted { len };
            }
        }
    }

    pub(crate) fn snapshot(&mut self, latest: SessionTime) -> NetSnapshot {
        if self.hosts_changed {
            self.hosts_cache = self.hosts.clone().into();
            self.hosts_changed = false;
        }
        NetSnapshot {
            requests: self.requests.snapshot(),
            sockets: self.sockets.snapshot(),
            hosts: self.hosts_cache.clone(),
            body_bytes: self.body_bytes,
            latest,
        }
    }
}

#[derive(Clone, Debug)]
pub struct NetSnapshot {
    requests: ChunkedSnapshot<Arc<HttpRecord>>,
    sockets: ChunkedSnapshot<SocketRecord>,
    hosts: Arc<[String]>,
    body_bytes: usize,
    latest: SessionTime,
}

impl Default for NetSnapshot {
    fn default() -> Self {
        Self {
            requests: ChunkedSnapshot::default(),
            sockets: ChunkedSnapshot::default(),
            hosts: Arc::from(Vec::new()),
            body_bytes: 0,
            latest: SessionTime(0),
        }
    }
}

impl NetSnapshot {
    pub fn len(&self) -> usize {
        self.requests.len()
    }

    pub fn is_empty(&self) -> bool {
        self.requests.is_empty()
    }

    /// The request with this sequence number; panics when out of range.
    pub fn request(&self, seq: usize) -> &Arc<HttpRecord> {
        self.requests.get(seq).expect("request seq in range")
    }

    pub fn get(&self, seq: usize) -> Option<&Arc<HttpRecord>> {
        self.requests.get(seq)
    }

    pub fn sockets(&self) -> &ChunkedSnapshot<SocketRecord> {
        &self.sockets
    }

    /// The socket carrying this websocket request, if one was linked.
    pub fn socket_for(&self, request: EntryId) -> Option<&SocketRecord> {
        self.sockets
            .chunks()
            .flatten()
            .find(|s| s.websocket == Some(request))
    }

    /// Distinct hosts in first-seen order.
    pub fn hosts(&self) -> Arc<[String]> {
        self.hosts.clone()
    }

    /// Bytes of bodies currently stored.
    pub fn body_bytes(&self) -> usize {
        self.body_bytes
    }

    /// The session's latest time, used as the end of pending requests.
    pub fn latest(&self) -> SessionTime {
        self.latest
    }

    pub(crate) fn chunks(&self) -> impl Iterator<Item = &[Arc<HttpRecord>]> {
        self.requests.chunks()
    }

    pub(crate) fn chunk_size(&self) -> usize {
        self.requests.chunk_size()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::testkit::writer;
    use crate::time::{EntryId, RunId, SessionTime};

    const MS: u64 = 1_000_000;

    fn http(run: RunId, vm_id: &str, url: &str, start_ms: u64) -> HttpUpdate {
        HttpUpdate {
            run,
            isolate: "isolates/1".into(),
            vm_id: vm_id.into(),
            method: "GET".into(),
            url: url.into(),
            start: SessionTime(start_ms * MS),
            start_clamped: false,
            request_sent: None,
            response_start: None,
            end: None,
            status: None,
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

    fn socket(run: RunId, vm_id: &str, addr: &str, port: u16, start_ms: u64) -> SocketUpdate {
        SocketUpdate {
            run,
            isolate: "isolates/1".into(),
            vm_id: vm_id.into(),
            address: addr.into(),
            port,
            kind: "tcp".into(),
            start: SessionTime(start_ms * MS),
            end: None,
            read_bytes: 0,
            write_bytes: 0,
            last_read: None,
            last_write: None,
        }
    }

    #[test]
    fn upserting_the_same_request_updates_in_place() {
        let (mut w, run) = writer(4);
        let a = w.upsert_request(http(run, "7", "http://h/a", 1));
        assert!(!a.newly_completed);
        let mut u = http(run, "7", "http://h/a", 1);
        u.status = Some(200);
        let b = w.upsert_request(u.clone());
        assert_eq!(a.id, b.id);
        assert!(!b.newly_completed);
        u.end = Some(SessionTime(9 * MS));
        let c = w.upsert_request(u.clone());
        assert!(c.newly_completed);
        let d = w.upsert_request(u);
        assert!(!d.newly_completed);
        let s = w.publish();
        assert_eq!(s.net.len(), 1);
        assert_eq!(s.net.request(0).status, Some(200));
        assert_eq!(s.net.request(0).id, EntryId { run, seq: 0 });
    }

    #[test]
    fn same_vm_id_in_a_new_isolate_is_a_new_request() {
        let (mut w, run) = writer(4);
        let a = w.upsert_request(http(run, "1", "http://h/a", 1));
        let mut u = http(run, "1", "http://h/b", 50);
        u.isolate = "isolates/2".into();
        let b = w.upsert_request(u);
        assert_ne!(a.id, b.id);
        let s = w.publish();
        assert_eq!(s.net.len(), 2);
        assert_eq!(s.net.request(0).url, "http://h/a");
        let mut s1 = socket(run, "1", "h", 1, 0);
        let id1 = w.upsert_socket(s1.clone());
        s1.isolate = "isolates/2".into();
        assert_ne!(id1, w.upsert_socket(s1));
    }

    #[test]
    fn host_and_path_split_cases() {
        assert_eq!(
            host_and_path_split("http://127.0.0.1:53452/v2/products?page=3"),
            (
                "127.0.0.1:53452".to_string(),
                "/v2/products?page=3".to_string()
            )
        );
        assert_eq!(
            host_and_path_split("not a url"),
            (String::new(), "not a url".to_string())
        );
        assert_eq!(
            host_and_path_split("https://user:pw@Example.com"),
            ("example.com".to_string(), "/".to_string())
        );
        assert_eq!(
            host_and_path_split("https://a.io?x=1#frag"),
            ("a.io".to_string(), "?x=1".to_string())
        );
    }

    #[test]
    fn status_classes_and_errors() {
        let (mut w, run) = writer(4);
        let mut mk = |id: &str, status: Option<u16>, err: Option<&str>, ws: bool| {
            let mut u = http(run, id, "http://h/x", 1);
            u.status = status;
            u.error = err.map(String::from);
            u.websocket = ws;
            w.upsert_request(u);
        };
        mk("1", Some(101), Some("Socket has been detached"), true);
        mk("2", Some(200), None, false);
        mk("3", Some(302), None, false);
        mk("4", Some(404), None, false);
        mk("5", Some(503), None, false);
        mk("6", None, Some("SocketException"), false);
        mk("7", None, None, false);
        mk("8", Some(100), None, false);
        let s = w.publish();
        let c: Vec<_> = (0..8).map(|i| s.net.request(i).status_class()).collect();
        assert_eq!(
            c,
            vec![
                StatusClass::Informational,
                StatusClass::Success,
                StatusClass::Redirect,
                StatusClass::ClientError,
                StatusClass::ServerError,
                StatusClass::Failed,
                StatusClass::Pending,
                StatusClass::Informational,
            ]
        );
        let e: Vec<_> = (0..8).map(|i| s.net.request(i).is_error()).collect();
        assert_eq!(e, vec![false, false, false, true, true, true, false, false]);
    }

    #[test]
    fn duration_size_and_content_type() {
        let (mut w, run) = writer(4);
        let mut u = http(run, "1", "http://h/x", 10);
        u.end = Some(SessionTime(35 * MS));
        u.response_headers = vec![("Content-Type".into(), vec!["image/png".into()])];
        u.response_content_length = Some(42);
        w.upsert_request(u);
        let mut u2 = http(run, "2", "http://h/y", 10);
        u2.response_content_length = Some(-1);
        let id2 = w.upsert_request(u2).id;
        w.store_body(id2, BodyKind::Response, vec![0; 7]);
        let s = w.publish();
        let r = s.net.request(0);
        assert_eq!(r.duration_ns(), Some(25 * MS));
        assert_eq!(r.size(), Some(42));
        assert_eq!(r.content_type(), Some("image/png"));
        let r2 = s.net.request(1);
        assert_eq!(r2.duration_ns(), None);
        assert_eq!(r2.size(), Some(7));
        assert_eq!(r2.content_type(), None);
    }

    #[test]
    fn bodies_over_the_cap_are_truncated() {
        let (mut w, run) = writer(4);
        let id = w.upsert_request(http(run, "1", "http://h/x", 1)).id;
        w.store_body(id, BodyKind::Response, vec![7u8; 2 << 20]);
        w.store_body(id, BodyKind::Request, vec![1u8; 10]);
        let s = w.publish();
        match &s.net.request(0).response_body {
            BodyState::Captured(b) => {
                assert_eq!(b.bytes.len(), 1 << 20);
                assert_eq!(b.full_len, 2 << 20);
                assert!(b.truncated());
            }
            other => panic!("{other:?}"),
        }
        match &s.net.request(0).request_body {
            BodyState::Captured(b) => assert!(!b.truncated()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn oldest_bodies_are_evicted_to_stay_in_budget() {
        let (mut w, run) = writer(4);
        w.set_body_limits(BodyLimits {
            per_body: 1 << 20,
            total: 3 << 20,
        });
        let ids: Vec<_> = (0..4)
            .map(|i| {
                w.upsert_request(http(run, &i.to_string(), "http://h/x", 1))
                    .id
            })
            .collect();
        for id in &ids {
            w.store_body(*id, BodyKind::Response, vec![0u8; 1 << 20]);
        }
        let s = w.publish();
        assert!(matches!(
            s.net.request(0).response_body,
            BodyState::Evicted { len } if len == 1 << 20
        ));
        for i in 1..4 {
            assert!(matches!(
                s.net.request(i).response_body,
                BodyState::Captured(_)
            ));
        }
        assert_eq!(s.net.body_bytes(), 3 << 20);
        // Replacing a stored body frees its share.
        w.set_body_state(
            ids[1],
            BodyKind::Response,
            BodyState::Unavailable {
                reason: "gone".into(),
            },
        );
        w.store_body(ids[0], BodyKind::Request, vec![0u8; 1 << 20]);
        let s = w.publish();
        assert_eq!(s.net.body_bytes(), 3 << 20);
        assert!(matches!(
            s.net.request(2).response_body,
            BodyState::Captured(_)
        ));
    }

    #[test]
    fn websocket_socket_is_linked_by_remote_address() {
        let (mut w, run) = writer(4);
        let mut near = socket(run, "s1", "10.0.0.2", 8080, 1_000);
        let far = socket(run, "s2", "10.0.0.2", 8080, 2_500);
        let other = socket(run, "s3", "10.0.0.9", 8080, 1_000);
        // Socket first, request second.
        let near_id = w.upsert_socket(near.clone());
        w.upsert_socket(far);
        w.upsert_socket(other);
        let mut r = http(run, "1", "http://10.0.0.2:8080/ws", 1_200);
        r.websocket = true;
        r.status = Some(101);
        r.remote = Some(("10.0.0.2".into(), 8080));
        let rid = w.upsert_request(r).id;
        near.read_bytes = 5;
        w.upsert_socket(near);
        let s = w.publish();
        let linked = s.net.socket_for(rid).unwrap();
        assert_eq!(linked.id, near_id);
        assert_eq!(linked.websocket, Some(rid));
        assert_eq!(linked.read_bytes, 5);
        assert!(
            s.net
                .sockets()
                .chunks()
                .flatten()
                .filter(|x| x.websocket.is_some())
                .count()
                == 1
        );

        // Request first, socket second.
        let mut r2 = http(run, "2", "http://10.0.0.3:9/ws", 10_000);
        r2.websocket = true;
        r2.remote = Some(("10.0.0.3".into(), 9));
        let rid2 = w.upsert_request(r2).id;
        assert!(w.publish().net.socket_for(rid2).is_none());
        let sid = w.upsert_socket(socket(run, "s9", "10.0.0.3", 9, 11_000));
        let s = w.publish();
        assert_eq!(s.net.socket_for(rid2).unwrap().id, sid);
        // Beyond 2 s does not link.
        let mut r3 = http(run, "3", "http://10.0.0.4:9/ws", 20_000);
        r3.websocket = true;
        r3.remote = Some(("10.0.0.4".into(), 9));
        let rid3 = w.upsert_request(r3).id;
        w.upsert_socket(socket(run, "s10", "10.0.0.4", 9, 22_500));
        assert!(w.publish().net.socket_for(rid3).is_none());
    }

    #[test]
    fn hosts_are_distinct_in_first_seen_order_and_cached() {
        let (mut w, run) = writer(4);
        for (i, u) in ["http://b/1", "http://a/1", "http://b/2"]
            .iter()
            .enumerate()
        {
            w.upsert_request(http(run, &i.to_string(), u, 1));
        }
        let s1 = w.publish();
        assert_eq!(&*s1.net.hosts(), ["b".to_string(), "a".to_string()]);
        w.upsert_request(http(run, "9", "http://a/9", 2));
        let s2 = w.publish();
        assert!(Arc::ptr_eq(&s1.net.hosts(), &s2.net.hosts()));
        w.upsert_request(http(run, "10", "http://c/1", 2));
        assert_eq!(w.publish().net.hosts().len(), 3);
    }

    #[test]
    fn latest_time_covers_request_and_socket_times() {
        let (mut w, run) = writer(4);
        let mut u = http(run, "1", "http://h/x", 5);
        u.end = Some(SessionTime(90 * MS));
        w.upsert_request(u);
        assert_eq!(w.publish().latest_time(), SessionTime(90 * MS));
        let mut s = socket(run, "s", "h", 1, 3);
        s.last_read = Some(SessionTime(120 * MS));
        w.upsert_socket(s);
        let snap = w.publish();
        assert_eq!(snap.latest_time(), SessionTime(120 * MS));
        assert_eq!(snap.net.latest(), SessionTime(120 * MS));
    }

    #[test]
    fn snapshots_are_isolated_from_later_updates_in_sealed_chunks() {
        let (mut w, run) = writer(2);
        for i in 0..5 {
            w.upsert_request(http(run, &i.to_string(), "http://h/x", 1));
        }
        let before = w.publish();
        let mut u = http(run, "0", "http://h/x", 1);
        u.status = Some(500);
        w.upsert_request(u);
        let after = w.publish();
        assert_eq!(before.net.request(0).status, None);
        assert_eq!(after.net.request(0).status, Some(500));
    }
}
