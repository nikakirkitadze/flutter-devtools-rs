//! What a log line or a request has in common with frames, computed off the UI thread.
//!
//! The Logs and Network details panes each hold a [`RelatedState`]. `sync` starts
//! `fdt_analysis::correlate` in `cx.background_spawn` for the selected record, keyed by the
//! session, the record's id and the snapshot generation, behind a 250 ms throttle with a trailing
//! rerun. A result for another key is dropped, so a pane's first draw never waits for it.

use crate::tools::network::model::{format_duration_ns, format_time, status_text};
use fdt_analysis::correlate::{related_to_log, related_to_request};
use fdt_store::Snapshot;
use fdt_store::frames::{FrameStatus, frame_status};
use fdt_store::time::EntryId;
use fdt_ui::tokens::{Palette, radius, space};
use gpui_kit::{
    AppContext as _, Context, Hsla, InteractiveElement as _, IntoElement, ParentElement,
    SharedString, StatefulInteractiveElement, Styled, Task, TestSupportExt as _, div,
};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The most links of one kind a pane shows.
pub const SHOWN: usize = 5;
/// The shortest time between two runs for the same record, as in the frame details.
const REFRESH_INTERVAL: Duration = Duration::from_millis(250); // token-check: allow: throttle, not a visual duration
const DASH: &str = "\u{2014}";

/// A clickable row in a Related block: accent text, pointer cursor, hover background.
pub fn link(
    id: SharedString,
    text: String,
    t: &Palette,
) -> impl IntoElement + Styled + ParentElement + StatefulInteractiveElement {
    div()
        .id(id)
        .test_support()
        .aria_label(SharedString::from(text.clone()))
        .w_full()
        .truncate()
        .px(space::S4)
        .rounded(radius::CONTROL)
        .text_color(Hsla::from(t.accent))
        .cursor_pointer()
        .hover(|s| s.bg(Hsla::from(t.elevated)))
        .child(text)
}

pub fn status_label(status: FrameStatus) -> &'static str {
    match status {
        FrameStatus::Ok => "OK",
        FrameStatus::Slow => "Slow",
        FrameStatus::Janky => "Janky",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Log,
    Request,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameLink {
    pub id: EntryId,
    pub number: u64,
    pub status: FrameStatus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub id: EntryId,
    pub text: String,
}

/// What the Related block shows, all text already formatted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RelatedView {
    pub frame: Option<FrameLink>,
    pub requests: Vec<Link>,
    pub logs: Vec<Link>,
}

/// The frame at a log line, as a link. Binary searches only, so a visible row may call it.
pub fn frame_for_log(snap: &Snapshot, log_seq: usize) -> Option<FrameLink> {
    let i = fdt_analysis::correlate::frame_for_log(snap, log_seq)?;
    frame_link(snap, i)
}

fn frame_link(snap: &Snapshot, seq: usize) -> Option<FrameLink> {
    let f = snap.frames.get(seq)?;
    Some(FrameLink {
        id: f.id,
        number: f.number,
        status: frame_status(f.time_us(), snap.frame_budget_us(f.run)),
    })
}

/// Runs the correlation. O(records in the window); only called off the UI thread.
pub fn compute(snap: &Snapshot, kind: Kind, seq: usize) -> RelatedView {
    let related = match kind {
        Kind::Log => related_to_log(snap, seq),
        Kind::Request => related_to_request(snap, seq),
    };
    let requests = related
        .requests
        .iter()
        .filter_map(|&s| snap.net.get(s as usize))
        .take(SHOWN)
        .map(|r| Link {
            id: r.id,
            text: format!(
                "{} {} \u{b7} {} \u{b7} {}",
                r.method,
                r.path,
                status_text(r),
                r.duration_ns().map_or(DASH.to_string(), format_duration_ns)
            ),
        })
        .collect();
    let logs = related
        .logs
        .iter()
        .filter_map(|&s| snap.logs.get_row(s as usize))
        .take(SHOWN)
        .map(|r| Link {
            id: r.id,
            text: format!(
                "{} \u{b7} {} \u{b7} {}",
                format_time(r.time),
                r.level.label(),
                elide(r.text.lines().next().unwrap_or(""))
            ),
        })
        .collect();
    RelatedView {
        frame: related.frames.first().and_then(|&i| frame_link(snap, i)),
        requests,
        logs,
    }
}

fn elide(line: &str) -> String {
    if line.chars().nth(80).is_none() {
        return line.to_string();
    }
    let mut s: String = line.chars().take(79).collect();
    s.push('\u{2026}');
    s
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Key {
    /// Run ids restart in every session, so the session is part of the key.
    session: u64,
    id: EntryId,
    generation: u64,
}

/// A details pane's related block.
#[derive(Default)]
pub struct RelatedState {
    started: Option<Key>,
    last_start: Option<Instant>,
    trailing: Option<Task<()>>,
    job: Option<Task<()>>,
    result: Option<(Key, Arc<RelatedView>)>,
    runs: usize,
}

impl RelatedState {
    pub fn clear(&mut self) {
        *self = Self {
            runs: self.runs,
            ..Self::default()
        };
    }

    /// The result for `id`, kept (as of an older snapshot) while a newer one is computed.
    pub fn view(&self, session: u64, id: EntryId) -> Option<Arc<RelatedView>> {
        self.result
            .as_ref()
            .filter(|(k, _)| k.session == session && k.id == id)
            .map(|(_, v)| v.clone())
    }

    #[cfg(test)]
    pub fn runs(&self) -> usize {
        self.runs
    }

    #[cfg(test)]
    fn with_result(session: u64, id: EntryId, view: RelatedView) -> Self {
        let key = Key {
            session,
            id,
            generation: 1,
        };
        Self {
            result: Some((key, Arc::new(view))),
            ..Self::default()
        }
    }
}

impl RelatedState {
    /// Starts the correlation for `target` (the record's id and its seq in the store) unless one
    /// for this snapshot is current, at most once per interval for the same record.
    ///
    /// `state` finds this `RelatedState` in the view; `rerun` is the view's own method that calls
    /// this again, for the trailing run.
    pub fn sync<V: 'static>(
        &mut self,
        cx: &mut Context<V>,
        state: fn(&mut V) -> &mut RelatedState,
        rerun: fn(&mut V, &mut Context<V>),
        snap: Arc<Snapshot>,
        target: (EntryId, usize),
        kind: Kind,
    ) {
        let (id, seq) = target;
        let key = Key {
            session: snap.session,
            id,
            generation: snap.generation,
        };
        if self.started == Some(key) {
            return;
        }
        let same_record = self
            .started
            .is_some_and(|k| k.session == key.session && k.id == key.id);
        let now = cx.background_executor().now();
        if same_record && let Some(at) = self.last_start {
            let since = now.saturating_duration_since(at);
            if since < REFRESH_INTERVAL {
                if self.trailing.is_none() {
                    let wait = REFRESH_INTERVAL - since;
                    self.trailing = Some(cx.spawn(async move |this, cx| {
                        cx.background_executor().timer(wait).await;
                        this.update(cx, |this, cx| {
                            state(this).trailing = None;
                            rerun(this, cx);
                        })
                        .ok();
                    }));
                }
                return;
            }
        }
        if !same_record {
            self.result = None;
        }
        self.runs += 1;
        self.trailing = None;
        self.last_start = Some(now);
        self.started = Some(key);
        self.job = Some(cx.spawn(async move |this, cx| {
            let view = cx
                .background_spawn(async move { compute(&snap, kind, seq) })
                .await;
            this.update(cx, |this, cx| {
                let st = state(this);
                // A newer run (another record, or the same one later) replaced this one.
                if st.started == Some(key) {
                    st.result = Some((key, Arc::new(view)));
                    cx.notify();
                }
            })
            .ok();
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fdt_store::time::RunId;

    #[test]
    fn a_result_is_only_shown_for_its_session_and_record() {
        let id = EntryId {
            run: RunId(1),
            seq: 5,
        };
        let st = RelatedState::with_result(1, id, RelatedView::default());
        assert!(st.view(1, id).is_some());
        // A new session reuses the id: the old result must not show.
        assert!(st.view(2, id).is_none());
        assert!(st.view(1, EntryId { seq: 6, ..id }).is_none());
    }
}
