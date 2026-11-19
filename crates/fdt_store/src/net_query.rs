//! Filtering HTTP requests, in parallel over chunks.

use crate::net::{HttpRecord, NetSnapshot, StatusClass};
use crate::time::TimeRange;
use rayon::prelude::*;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NetTab {
    #[default]
    Requests,
    WebSockets,
    Errors,
}

const METHODS: [&str; 5] = ["GET", "POST", "PUT", "PATCH", "DELETE"];
const OTHER: u8 = 5;

fn method_index(m: &str) -> u8 {
    METHODS
        .iter()
        .position(|k| k.eq_ignore_ascii_case(m))
        .map_or(OTHER, |i| i as u8)
}

/// A bitset over `GET POST PUT PATCH DELETE OTHER`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MethodSet(u8);

impl MethodSet {
    pub const ALL: MethodSet = MethodSet(0b11_1111);
    pub const NONE: MethodSet = MethodSet(0);

    /// The set of these methods; names that are not listed count as `OTHER`.
    pub fn only(methods: &[&str]) -> Self {
        Self(methods.iter().fold(0, |acc, m| acc | 1 << method_index(m)))
    }

    pub fn contains(self, method: &str) -> bool {
        self.0 & (1 << method_index(method)) != 0
    }

    pub fn toggle(&mut self, method: &str) {
        self.0 ^= 1 << method_index(method);
    }
}

impl Default for MethodSet {
    fn default() -> Self {
        Self::ALL
    }
}

/// A bitset over the `StatusClass` values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClassSet(u8);

impl ClassSet {
    pub const ALL: ClassSet = ClassSet(0b111_1111);
    pub const NONE: ClassSet = ClassSet(0);

    pub fn contains(self, c: StatusClass) -> bool {
        self.0 & (1 << c.index()) != 0
    }

    pub fn toggle(&mut self, c: StatusClass) {
        self.0 ^= 1 << c.index();
    }
}

impl Default for ClassSet {
    fn default() -> Self {
        Self::ALL
    }
}

#[derive(Clone, Debug, Default)]
pub struct NetFilter {
    pub tab: NetTab,
    pub methods: MethodSet,
    pub classes: ClassSet,
    /// Exact host (authority) match.
    pub host: Option<String>,
    /// Case-insensitive substring of the URL.
    pub text: String,
    pub range: Option<TimeRange>,
}

/// A case-insensitive substring test, allocation-free for ASCII needles.
struct Needle {
    lower: String,
    ascii: bool,
}

impl Needle {
    fn new(text: &str) -> Self {
        Self {
            lower: text.to_lowercase(),
            ascii: text.is_ascii(),
        }
    }

    fn is_empty(&self) -> bool {
        self.lower.is_empty()
    }

    fn found_in(&self, hay: &str) -> bool {
        if !self.ascii {
            return hay.to_lowercase().contains(&self.lower);
        }
        let (h, n) = (hay.as_bytes(), self.lower.as_bytes());
        if h.len() < n.len() {
            return false;
        }
        let first = n[0];
        (0..=h.len() - n.len()).any(|i| {
            h[i].to_ascii_lowercase() == first && h[i..i + n.len()].eq_ignore_ascii_case(n)
        })
    }
}

/// Sequence numbers of the matching requests, oldest first.
pub fn filter_requests(snap: &NetSnapshot, f: &NetFilter) -> Vec<u32> {
    let chunks: Vec<&[Arc<HttpRecord>]> = snap.chunks().collect();
    let size = snap.chunk_size();
    let latest = snap.latest();
    let needle = Needle::new(&f.text);
    let parts: Vec<Vec<u32>> = chunks
        .par_iter()
        .enumerate()
        .map(|(ci, chunk)| {
            let base = ci * size;
            chunk
                .iter()
                .enumerate()
                .filter(|(_, r)| keep(r, f, &needle, latest))
                .map(|(o, _)| (base + o) as u32)
                .collect()
        })
        .collect();
    parts.concat()
}

fn keep(r: &HttpRecord, f: &NetFilter, needle: &Needle, latest: crate::time::SessionTime) -> bool {
    match f.tab {
        NetTab::Requests => {}
        NetTab::WebSockets if !r.websocket => return false,
        NetTab::Errors if !r.is_error() => return false,
        _ => {}
    }
    f.classes.contains(r.status_class())
        && f.methods.contains(&r.method)
        && f.host.as_deref().is_none_or(|h| r.host == h)
        && (needle.is_empty() || needle.found_in(&r.url))
        && f.range
            .is_none_or(|g| g.overlaps(r.start, r.end.unwrap_or(latest).max(r.start)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::testkit::{p90_worst, serial, writer};
    use crate::net::{HttpUpdate, StatusClass};
    use crate::time::{RunId, SessionTime, TimeRange};
    use std::time::Instant;

    const MS: u64 = 1_000_000;

    fn req(
        w: &mut crate::SessionWriter,
        run: RunId,
        method: &str,
        url: &str,
        status: Option<u16>,
        start_ms: u64,
        end_ms: Option<u64>,
    ) {
        w.upsert_request(HttpUpdate {
            run,
            isolate: "i".into(),
            vm_id: start_ms.to_string(),
            method: method.into(),
            url: url.into(),
            start: SessionTime(start_ms * MS),
            start_clamped: false,
            request_sent: None,
            response_start: None,
            end: end_ms.map(|m| SessionTime(m * MS)),
            status,
            reason: None,
            request_headers: vec![],
            response_headers: vec![],
            request_content_length: None,
            response_content_length: None,
            events: vec![],
            error: None,
            websocket: status == Some(101),
            remote: None,
        });
    }

    #[test]
    fn filters_by_method_class_host_text_range_and_tab() {
        let (mut w, run) = writer(2);
        req(
            &mut w,
            run,
            "GET",
            "http://a.com/Products?p=1",
            Some(200),
            0,
            Some(10),
        );
        req(
            &mut w,
            run,
            "POST",
            "http://a.com/orders",
            Some(500),
            20,
            Some(30),
        );
        req(
            &mut w,
            run,
            "get",
            "http://b.com/products",
            Some(404),
            40,
            Some(50),
        );
        req(
            &mut w,
            run,
            "OPTIONS",
            "http://b.com/x",
            Some(101),
            60,
            Some(70),
        );
        req(&mut w, run, "DELETE", "http://b.com/y", None, 80, None);
        let s = w.publish();
        let f = |f: NetFilter| filter_requests(&s.net, &f);
        let base = NetFilter::default();
        assert_eq!(f(base.clone()), vec![0, 1, 2, 3, 4]);
        assert_eq!(
            f(NetFilter {
                text: "PRODUCTS".into(),
                ..base.clone()
            }),
            vec![0, 2]
        );
        assert_eq!(
            f(NetFilter {
                methods: MethodSet::only(&["POST", "OPTIONS"]),
                ..base.clone()
            }),
            vec![1, 3]
        );
        let mut errs = ClassSet::NONE;
        errs.toggle(StatusClass::ClientError);
        errs.toggle(StatusClass::ServerError);
        assert_eq!(
            f(NetFilter {
                classes: errs,
                ..base.clone()
            }),
            vec![1, 2]
        );
        assert_eq!(
            f(NetFilter {
                host: Some("b.com".into()),
                ..base.clone()
            }),
            vec![2, 3, 4]
        );
        assert_eq!(
            f(NetFilter {
                tab: NetTab::WebSockets,
                ..base.clone()
            }),
            vec![3]
        );
        assert_eq!(
            f(NetFilter {
                tab: NetTab::Errors,
                ..base.clone()
            }),
            vec![1, 2]
        );
        // Range overlap; the pending request runs to the latest time (80 ms).
        let r = |a: u64, b: u64| NetFilter {
            range: Some(TimeRange::new(SessionTime(a * MS), SessionTime(b * MS))),
            ..base.clone()
        };
        assert_eq!(f(r(11, 19)), Vec::<u32>::new());
        assert_eq!(f(r(10, 20)), vec![0, 1]);
        assert_eq!(f(r(85, 90)), Vec::<u32>::new());
        assert_eq!(f(r(80, 90)), vec![4]);
    }

    #[test]
    fn pending_requests_extend_to_the_latest_time() {
        let (mut w, run) = writer(2);
        req(&mut w, run, "GET", "http://a/x", None, 0, None);
        req(&mut w, run, "GET", "http://a/y", Some(200), 50, Some(100));
        let s = w.publish();
        let f = NetFilter {
            range: Some(TimeRange::new(SessionTime(90 * MS), SessionTime(95 * MS))),
            ..NetFilter::default()
        };
        assert_eq!(filter_requests(&s.net, &f), vec![0, 1]);
    }

    #[test]
    fn method_and_class_sets() {
        let m = MethodSet::only(&["GET"]);
        assert!(m.contains("get") && !m.contains("POST") && !m.contains("TRACE"));
        assert!(MethodSet::ALL.contains("TRACE"));
        assert!(MethodSet::only(&["OTHER"]).contains("TRACE"));
        let mut c = ClassSet::ALL;
        c.toggle(StatusClass::Failed);
        assert!(!c.contains(StatusClass::Failed) && c.contains(StatusClass::Pending));
    }

    #[test]
    #[cfg_attr(debug_assertions, ignore)]
    fn net_filter_budget_100k() {
        let _g = serial();
        let (mut w, run) = writer(crate::logs::LOG_CHUNK);
        crate::synth::requests(&mut w, run, 100_000, 7);
        let s = w.publish();
        let host = s.net.hosts()[3].clone();
        let mut errors = ClassSet::NONE;
        errors.toggle(StatusClass::ClientError);
        errors.toggle(StatusClass::ServerError);
        let filters = [
            (
                "text products",
                NetFilter {
                    text: "products".into(),
                    ..NetFilter::default()
                },
            ),
            (
                "4xx and 5xx",
                NetFilter {
                    classes: errors,
                    ..NetFilter::default()
                },
            ),
            (
                "host and POST",
                NetFilter {
                    host: Some(host),
                    methods: MethodSet::only(&["POST"]),
                    ..NetFilter::default()
                },
            ),
            (
                "errors tab",
                NetFilter {
                    tab: NetTab::Errors,
                    ..NetFilter::default()
                },
            ),
        ];
        for (name, f) in filters {
            let times: Vec<f64> = (0..50)
                .map(|_| {
                    let t = Instant::now();
                    std::hint::black_box(filter_requests(&s.net, &f));
                    t.elapsed().as_secs_f64() * 1e3
                })
                .collect();
            let (p90, worst) = p90_worst(times);
            eprintln!("net filter {name}: p90 {p90:.2} ms, worst {worst:.2} ms");
            assert!(p90 <= 16.0, "{name}: p90 {p90:.2} ms");
        }
    }
}
