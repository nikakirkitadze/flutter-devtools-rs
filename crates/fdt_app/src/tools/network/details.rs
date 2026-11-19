//! The request details pane: header, copy buttons and collapsible sections for the overview,
//! headers, bodies, timing and error.
//!
//! Body previews are built off the UI thread ([`build`]) and cached per body, so a redraw never
//! recomputes them and a result for a request that is no longer selected is dropped.

use super::body::{BodyKindGuess, RenderedBody, guess, render_lines};
use super::model::{Tone, format_bytes, format_duration_ns, format_time, status_text, status_tone};
use super::view::NetworkView;
use crate::settings::settings;
use fdt_capture::ConnStatus;
use fdt_store::net::{Body, BodyKind, BodyState, HttpRecord};
use fdt_store::time::{EntryId, RunId};
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{alpha, font, layout, motion, radius, row, space};
use gpui_kit::component::Sizable as _;
use gpui_kit::component::Size;
use gpui_kit::component::resizable::{
    ResizablePanelEvent, ResizableState, h_resizable, resizable_panel,
};
use gpui_kit::component::skeleton::Skeleton;
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{
    AnyElement, AppContext as _, ClipboardItem, Context, Entity, Hsla, Image,
    InteractiveElement as _, IntoElement, ParentElement as _, Pixels, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledImage as _, Subscription, Task,
    TestSupportExt as _, UniformListScrollHandle, div, img, relative, uniform_list,
};
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

#[cfg(test)]
thread_local! {
    /// Body renders started, for the "cached per body" test.
    pub(crate) static RENDER_JOBS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The most body lines shown at once; the list scrolls inside this height.
const BODY_ROWS: usize = 24;
/// The most headers listed per section.
const HEADERS_MAX: usize = 200;
/// Images larger than this show the hex fallback instead of being decoded.
const IMAGE_PREVIEW_MAX: usize = 8 << 20;
const IMAGE_TOO_LARGE: &str = "Image too large to preview.";
/// Lines of a failed image's hex preview.
const FALLBACK_LINES: usize = 64;

pub const LOAD_FULL_HINT: &str = "Reconnect to the app to load the rest.";

/// The tooltip of "Load full body": there is one when it cannot be used.
pub fn load_full_hint(connected: bool) -> Option<&'static str> {
    (!connected).then_some(LOAD_FULL_HINT)
}

/// The largest body one frame can carry: the frame limit over the four characters a byte takes
/// in the VM's integer-array encoding (64 MiB at the default 256 MiB frame).
pub const LOAD_FULL_MAX_BYTES: u64 =
    fdt_vm::rpc::max_body_bytes(fdt_vm::rpc::DEFAULT_MAX_MESSAGE_BYTES) as u64;

/// The tooltip of a disabled "Load full body" when the body is known to be too long to carry
/// in one piece.
pub fn load_full_too_big_hint(full_len: Option<u64>) -> Option<String> {
    full_len.filter(|&n| n > LOAD_FULL_MAX_BYTES).map(|_| {
        format!(
            "Too large to copy ({} limit).",
            format_bytes(LOAD_FULL_MAX_BYTES)
        )
    })
}

/// What a finished preview job produced.
pub(super) struct Rendered {
    lines: Arc<Vec<String>>,
    note: Option<String>,
    /// Set when the body is an image the preview should draw.
    image: Option<Arc<Image>>,
}

/// Builds the preview of `body`. Pure; runs on the background executor.
fn build(content_type: Option<&str>, body: &Body) -> Rendered {
    let mut bytes: &[u8] = &body.bytes;
    let mut kind = guess(content_type, bytes);
    if body.truncated() {
        // A cut-off body is not a whole document or picture, and its notice says so: JSON is
        // shown as text without the "not valid JSON" note, an image as bytes.
        kind = match kind {
            BodyKindGuess::Json => BodyKindGuess::Text,
            BodyKindGuess::Image(_) => BodyKindGuess::Binary,
            other => other,
        };
        // A character cut in half by the cap is not an encoding problem.
        if let Err(e) = std::str::from_utf8(bytes)
            && e.error_len().is_none()
        {
            bytes = &bytes[..e.valid_up_to()];
        }
    }
    // A very large image is not copied and decoded for a preview.
    let too_large = matches!(kind, BodyKindGuess::Image(_)) && bytes.len() > IMAGE_PREVIEW_MAX;
    if too_large {
        kind = BodyKindGuess::Binary;
    }
    let image = match kind {
        BodyKindGuess::Image(format) => Some(Arc::new(Image::from_bytes(format, bytes.to_vec()))),
        _ => None,
    };
    let RenderedBody { lines, mut note } = render_lines(kind, bytes);
    if too_large {
        note = Some(IMAGE_TOO_LARGE.to_string());
    }
    Rendered {
        lines: Arc::new(lines),
        note,
        image,
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct BodyKey {
    id: EntryId,
    kind: BodyKind,
    /// The stored bytes' address and length: a reloaded body is a new key.
    bytes: (usize, usize),
    content_type: Option<String>,
}

enum Slot {
    Rendering {
        /// Set once the job has taken longer than `LOADING_DELAY`.
        skeleton: bool,
        _job: Task<()>,
        _timer: Task<()>,
    },
    Ready(Arc<Rendered>),
}

/// A wait that only shows after `LOADING_DELAY`.
struct Delayed {
    shown: bool,
    _task: Task<()>,
}

/// A "load the full body" request the view is waiting on.
struct Load {
    spinner: bool,
    _delay: Task<()>,
    _timeout: Task<()>,
}

/// The details pane's state.
pub(super) struct DetailsState {
    pub(super) open: bool,
    split: Option<Entity<ResizableState>>,
    width: Pixels,
    split_sub: Option<Subscription>,
    collapsed: HashSet<&'static str>,
    cache: HashMap<BodyKey, Slot>,
    /// The cache keys the last draw wanted; the rest are dropped.
    wanted: Vec<BodyKey>,
    pending: HashMap<(EntryId, BodyKind), Delayed>,
    loads: HashMap<(EntryId, BodyKind), Load>,
    scroll: [UniformListScrollHandle; 2],
    shown: Option<EntryId>,
}

impl DetailsState {
    pub(super) fn new() -> Self {
        Self {
            open: false,
            split: None,
            width: layout::DETAILS_WIDTH,
            split_sub: None,
            collapsed: HashSet::new(),
            cache: HashMap::new(),
            wanted: Vec::new(),
            pending: HashMap::new(),
            loads: HashMap::new(),
            scroll: [
                UniformListScrollHandle::new(),
                UniformListScrollHandle::new(),
            ],
            shown: None,
        }
    }

    fn toggle(&mut self, section: &'static str) {
        if !self.collapsed.remove(section) {
            self.collapsed.insert(section);
        }
    }

    fn finish(&mut self, key: &BodyKey, rendered: Rendered) {
        if let Some(slot @ Slot::Rendering { .. }) = self.cache.get_mut(key) {
            *slot = Slot::Ready(Arc::new(rendered));
        }
    }
}

fn kind_index(kind: BodyKind) -> usize {
    match kind {
        BodyKind::Request => 0,
        BodyKind::Response => 1,
    }
}

/// Element id prefix of a body: the response's keeps the plain `net-body` names.
fn prefix(kind: BodyKind) -> &'static str {
    match kind {
        BodyKind::Response => "net-body",
        BodyKind::Request => "net-request-body",
    }
}

fn sid(id: String) -> SharedString {
    SharedString::from(id)
}

/// Headers sorted by name, each name once with its values joined by ", ".
pub(super) fn sorted_headers(headers: &[(String, Vec<String>)]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = headers
        .iter()
        .map(|(name, values)| (name.clone(), values.join(", ")))
        .collect();
    out.sort_by_cached_key(|(name, _)| name.to_ascii_lowercase());
    out
}

/// What "Copy headers" writes: the request's `Name: value` lines, a blank line, the response's.
pub(super) fn headers_text(rec: &HttpRecord) -> String {
    let block = |headers: &[(String, Vec<String>)]| {
        sorted_headers(headers)
            .into_iter()
            .map(|(k, v)| format!("{k}: {v}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let (req, res) = (block(&rec.request_headers), block(&rec.response_headers));
    match (req.is_empty(), res.is_empty()) {
        (false, false) => format!("{req}\n\n{res}"),
        (true, _) => res,
        (_, true) => req,
    }
}

fn request_content_type(rec: &HttpRecord) -> Option<&str> {
    rec.request_headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        .and_then(|(_, v)| v.first())
        .map(String::as_str)
}

/// `+182 ms`.
fn offset_text(ns: u64) -> String {
    format!("+{}", format_duration_ns(ns))
}

impl NetworkView {
    // ---- opening, closing and the capture ----

    /// Shows the details pane for the selected request.
    pub fn open_details(&mut self, cx: &mut Context<Self>) {
        let Some(seq) = self.selected else { return };
        let Some(id) = self.snap.net.get(seq as usize).map(|r| r.id) else {
            return;
        };
        self.details.open = true;
        // Enter commits the selection as a history step (nothing new if it already is one).
        cx.emit(super::NetworkEvent::Selected(Some(id)));
        cx.notify();
    }

    /// Closes the details pane; true when it was open (Esc closes it before the selection).
    pub fn close_details(&mut self, cx: &mut Context<Self>) -> bool {
        let was = std::mem::take(&mut self.details.open);
        if was {
            cx.notify();
        }
        was
    }

    fn is_connected_run(&self, run: RunId, cx: &gpui_kit::App) -> bool {
        matches!(self.session.read(cx).status(), ConnStatus::Connected { run: r, .. } if *r == run)
    }

    /// Whether "Load full body" cannot be used: the selected request's run is not connected.
    #[cfg(test)]
    pub fn load_full_disabled(&self, cx: &gpui_kit::App) -> bool {
        self.selected
            .and_then(|s| self.snap.net.get(s as usize))
            .is_none_or(|r| !self.is_connected_run(r.id.run, cx))
    }

    /// Asks the capture for the rest of a body, once; the pane waits on it.
    pub(super) fn load_full(&mut self, id: EntryId, kind: BodyKind, cx: &mut Context<Self>) {
        if !self.is_connected_run(id.run, cx) || self.details.loads.contains_key(&(id, kind)) {
            return;
        }
        let Some(control) = self.session.read(cx).control() else {
            return;
        };
        control.load_full_body(id, kind);
        let delay = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(motion::LOADING_DELAY).await;
            this.update(cx, |this, cx| {
                if let Some(load) = this.details.loads.get_mut(&(id, kind)) {
                    load.spinner = true;
                    cx.notify();
                }
            })
            .ok();
        });
        // The capture keeps the stored body when it cannot reload it, and says nothing.
        let timeout = cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(motion::FULL_BODY_TIMEOUT)
                .await;
            this.update(cx, |this, cx| {
                this.details.loads.remove(&(id, kind));
                cx.notify();
            })
            .ok();
        });
        self.details.loads.insert(
            (id, kind),
            Load {
                spinner: false,
                _delay: delay,
                _timeout: timeout,
            },
        );
        cx.notify();
    }

    /// The palette's "Load Full Response Body": for the selected request, when it is offered.
    pub fn load_full_selected(&mut self, cx: &mut Context<Self>) {
        let Some(rec) = self.selected.and_then(|s| self.snap.net.get(s as usize)) else {
            return;
        };
        let (id, offered) = (rec.id, offers_full(&rec.response_body));
        if offered {
            self.load_full(id, BodyKind::Response, cx);
        }
    }

    /// Ends waits that are over: the body arrived whole, or the run is no longer connected.
    pub(super) fn details_session_changed(&mut self, cx: &mut Context<Self>) {
        if self.details.loads.is_empty() {
            return;
        }
        let snap = self.session.read(cx).snapshot().clone();
        let before = self.details.loads.len();
        let keys: Vec<(EntryId, BodyKind)> = self.details.loads.keys().copied().collect();
        for (id, kind) in keys {
            let whole = snap
                .net
                .get(id.seq as usize)
                .is_some_and(|r| matches!(r.body(kind), BodyState::Captured(b) if !b.truncated()));
            if whole || !self.is_connected_run(id.run, cx) {
                self.details.loads.remove(&(id, kind));
            }
        }
        if self.details.loads.len() != before && self.details.open {
            cx.notify();
        }
    }

    fn copy_body(&self, bytes: Arc<[u8]>, cx: &mut Context<Self>) {
        // Decoding up to the stored size is off the UI thread too.
        cx.spawn(async move |this, cx| {
            let text = cx
                .background_spawn(async move { String::from_utf8_lossy(&bytes).into_owned() })
                .await;
            this.update(cx, |_, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(text));
            })
            .ok();
        })
        .detach();
    }

    // ---- preview jobs ----

    /// The cached preview of `body`, starting its job when there is none.
    fn rendered(
        &mut self,
        rec: &HttpRecord,
        kind: BodyKind,
        body: &Body,
        cx: &mut Context<Self>,
    ) -> Option<Arc<Rendered>> {
        let content_type = match kind {
            BodyKind::Response => rec.content_type(),
            BodyKind::Request => request_content_type(rec),
        }
        .map(str::to_string);
        let key = BodyKey {
            id: rec.id,
            kind,
            bytes: (body.bytes.as_ptr() as usize, body.bytes.len()),
            content_type,
        };
        self.details.wanted.push(key.clone());
        match self.details.cache.get(&key) {
            Some(Slot::Ready(r)) => return Some(r.clone()),
            Some(Slot::Rendering { .. }) => return None,
            None => {}
        }
        #[cfg(test)]
        RENDER_JOBS.with(|c| c.set(c.get() + 1));
        let (job_key, body, ct) = (key.clone(), body.clone(), key.content_type.clone());
        let job = cx.spawn(async move |this, cx| {
            let rendered = cx
                .background_spawn(async move { build(ct.as_deref(), &body) })
                .await;
            this.update(cx, |this, cx| {
                this.details.finish(&job_key, rendered);
                cx.notify();
            })
            .ok();
        });
        let timer_key = key.clone();
        let timer = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(motion::LOADING_DELAY).await;
            this.update(cx, |this, cx| {
                if let Some(Slot::Rendering { skeleton, .. }) =
                    this.details.cache.get_mut(&timer_key)
                {
                    *skeleton = true;
                    cx.notify();
                }
            })
            .ok();
        });
        self.details.cache.insert(
            key,
            Slot::Rendering {
                skeleton: false,
                _job: job,
                _timer: timer,
            },
        );
        None
    }

    /// Whether a skeleton for waiting on `(id, kind)` shows yet; starts the wait's clock.
    fn pending_skeleton(&mut self, id: EntryId, kind: BodyKind, cx: &mut Context<Self>) -> bool {
        if let Some(d) = self.details.pending.get(&(id, kind)) {
            return d.shown;
        }
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(motion::LOADING_DELAY).await;
            this.update(cx, |this, cx| {
                if let Some(d) = this.details.pending.get_mut(&(id, kind)) {
                    d.shown = true;
                    cx.notify();
                }
            })
            .ok();
        });
        self.details.pending.insert(
            (id, kind),
            Delayed {
                shown: false,
                _task: task,
            },
        );
        false
    }

    // ---- layout ----

    /// `list` beside the details pane, in a split the user can resize.
    pub(super) fn with_details(&mut self, list: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        let state = match &self.details.split {
            Some(state) => state.clone(),
            None => {
                let state = cx.new(|_| ResizableState::default());
                self.details.split_sub = Some(cx.subscribe(
                    &state,
                    |this, state, _: &ResizablePanelEvent, cx| {
                        if let Some(&w) = state.read(cx).sizes().last() {
                            this.details.width = w;
                        }
                    },
                ));
                self.details.split = Some(state.clone());
                state
            }
        };
        let pane = self.render_details(cx);
        h_resizable("net-split")
            .with_state(&state)
            .child(resizable_panel().child(list))
            .child(
                resizable_panel()
                    .size(self.details.width)
                    .size_range(layout::DETAILS_MIN..layout::DETAILS_MAX)
                    .flex_none()
                    .child(pane),
            )
            .into_any_element()
    }

    fn mono_row() -> gpui_kit::Div {
        div()
            .flex_none()
            .w_full()
            .h(row::LOG)
            .font_family(font::MONO)
            .text_size(font::DENSE)
            .line_height(relative(font::LINE_HEIGHT_LOG))
            .whitespace_nowrap()
    }

    fn render_details(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let t = *cx.tokens();
        let shell = div()
            .id("net-details")
            .test_support()
            .size_full()
            .flex()
            .flex_col()
            .border_l(layout::BORDER)
            .border_color(Hsla::from(t.border))
            .bg(Hsla::from(t.surface))
            .text_size(font::BODY)
            .text_color(Hsla::from(t.text));
        let snap = self.session.read(cx).snapshot().clone();
        let rec = self
            .selected
            .and_then(|s| snap.net.get(s as usize))
            .cloned();
        let Some(rec) = rec else {
            self.details.wanted.clear();
            self.details.cache.clear();
            return shell
                .p(space::S12)
                .child(
                    div()
                        .text_color(Hsla::from(t.text_muted))
                        .child("Select a request to see its details."),
                )
                .into_any_element();
        };
        if self.details.shown != Some(rec.id) {
            self.details.shown = Some(rec.id);
            self.details.scroll = [
                UniformListScrollHandle::new(),
                UniformListScrollHandle::new(),
            ];
        }
        // Waits for bodies that are no longer pending end with them.
        self.details
            .pending
            .retain(|(id, kind), _| *id == rec.id && matches!(rec.body(*kind), BodyState::Pending));
        self.details.wanted.clear();
        let connected = self.is_connected_run(rec.id.run, cx);

        let tone = |tone: Tone| match tone {
            Tone::Success => Hsla::from(t.success),
            Tone::Muted => Hsla::from(t.text_muted),
            Tone::Warning => Hsla::from(t.warning),
            Tone::Danger => Hsla::from(t.danger),
        };
        let status = status_text(&rec);
        let duration = rec.duration_ns().map(format_duration_ns);
        let url = rec.url.clone();
        let headers_copy = headers_text(&rec);

        let header =
            div()
                .flex()
                .flex_col()
                .flex_none()
                .gap(space::S4)
                .p(space::S12)
                .border_b(layout::BORDER)
                .border_color(Hsla::from(t.border))
                .child(
                    div()
                        .flex()
                        .items_start()
                        .gap(space::S8)
                        .child(
                            div()
                                .id("net-details-method")
                                .test_support()
                                .aria_label(SharedString::from(rec.method.clone()))
                                .flex_none()
                                .font_weight(font::SEMIBOLD)
                                .child(rec.method.clone()),
                        )
                        .child(
                            div()
                                .id("net-details-url")
                                .test_support()
                                .aria_label(SharedString::from(url.clone()))
                                .flex_1()
                                .min_w_0()
                                .whitespace_normal()
                                .font_family(font::MONO)
                                .text_size(font::DENSE)
                                .child(url.clone()),
                        ),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(space::S8)
                        .child(
                            div()
                                .id("net-details-status")
                                .test_support()
                                .aria_label(SharedString::from(status.clone()))
                                .text_color(tone(status_tone(&rec)))
                                .child(status.clone()),
                        )
                        .children(duration.clone().map(|d| {
                            div()
                                .id("net-details-duration")
                                .test_support()
                                .aria_label(SharedString::from(d.clone()))
                                .text_color(Hsla::from(t.text_muted))
                                .child(d)
                        })),
                )
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(space::S8)
                        .child(
                            action_button("net-copy-url", "Copy URL", &t).on_click(cx.listener(
                                move |_, _, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
                                },
                            )),
                        )
                        .child(
                            action_button("net-copy-headers", "Copy headers", &t).on_click(
                                cx.listener(move |_, _, _, cx| {
                                    cx.write_to_clipboard(ClipboardItem::new_string(
                                        headers_copy.clone(),
                                    ));
                                }),
                            ),
                        )
                        .child(div().flex_1())
                        .child(action_button("net-details-close", "Close", &t).on_click(
                            cx.listener(|this, _, _, cx| {
                                this.close_details(cx);
                            }),
                        )),
                );

        let mut sections: Vec<AnyElement> = Vec::new();
        sections.push(self.section(
            "overview",
            "Overview",
            None,
            |this, cx| this.overview(&rec, cx),
            cx,
        ));
        if rec.error.is_some() {
            sections.push(self.section("error", "Error", None, |_, cx| error_body(&rec, cx), cx));
        }
        sections.push(self.section(
            "request-headers",
            "Request headers",
            None,
            |_, cx| headers_body("net-request-headers", &rec.request_headers, cx),
            cx,
        ));
        sections.push(self.section(
            "response-headers",
            "Response headers",
            None,
            |_, cx| headers_body("net-response-headers", &rec.response_headers, cx),
            cx,
        ));
        for (section, title, kind) in [
            ("request-body", "Request body", BodyKind::Request),
            ("response-body", "Response body", BodyKind::Response),
        ] {
            let copy = match rec.body(kind) {
                BodyState::Captured(b) => Some((kind, b.bytes.clone())),
                _ => None,
            };
            sections.push(self.section(
                section,
                title,
                copy,
                |this, cx| this.body_content(&rec, kind, connected, cx),
                cx,
            ));
        }
        sections.push(self.section("timing", "Timing", None, |_, cx| timing_body(&rec, cx), cx));
        let wanted = std::mem::take(&mut self.details.wanted);
        self.details.cache.retain(|k, _| wanted.contains(k));
        self.details.wanted = wanted;

        shell
            .child(header)
            .child(
                div()
                    .id("net-details-scroll")
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .children(sections),
            )
            .into_any_element()
    }

    /// A collapsible section; `copy` adds a "Copy body" button for that body.
    fn section(
        &mut self,
        id: &'static str,
        title: &'static str,
        copy: Option<(BodyKind, Arc<[u8]>)>,
        content: impl FnOnce(&mut Self, &mut Context<Self>) -> AnyElement,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = *cx.tokens();
        let collapsed = self.details.collapsed.contains(id);
        let toggle = div()
            .id(sid(format!("net-sec-{id}")))
            .test_support()
            .aria_label(title)
            .flex_1()
            .flex()
            .items_center()
            .gap(space::S6)
            .cursor_pointer()
            .text_size(font::LABEL)
            .font_weight(font::SEMIBOLD)
            .text_color(Hsla::from(t.text_muted))
            .on_click(cx.listener(move |this, _, _, cx| {
                this.details.toggle(id);
                cx.notify();
            }))
            .child(if collapsed { "\u{25b8}" } else { "\u{25be}" })
            .child(title);
        let copy_button = copy.map(|(kind, bytes)| {
            let button_id = match kind {
                BodyKind::Response => "net-copy-body",
                BodyKind::Request => "net-request-copy-body",
            };
            action_button(button_id, "Copy body", &t)
                .on_click(cx.listener(move |this, _, _, cx| this.copy_body(bytes.clone(), cx)))
        });
        let body = (!collapsed).then(|| content(self, cx));
        div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(space::S6)
            .px(space::S12)
            .py(space::S8)
            .border_b(layout::BORDER)
            .border_color(Hsla::from(t.border))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::S8)
                    .child(toggle)
                    .children(copy_button),
            )
            .children(body)
            .into_any_element()
    }

    fn overview(&mut self, rec: &HttpRecord, cx: &mut Context<Self>) -> AnyElement {
        let t = *cx.tokens();
        let dash = "\u{2013}".to_string();
        let run = format!("Run {}", rec.id.run.0);
        div()
            .flex()
            .flex_col()
            .gap(space::S2)
            .child(meta(
                "net-overview-start",
                "Start",
                format_time(rec.start),
                &t,
            ))
            .child(meta(
                "net-overview-duration",
                "Duration",
                rec.duration_ns()
                    .map_or_else(|| dash.clone(), format_duration_ns),
                &t,
            ))
            .child(meta(
                "net-overview-size",
                "Size",
                rec.size().map_or(dash, format_bytes),
                &t,
            ))
            .child(meta("net-overview-run", "Run", run, &t))
            .children(rec.before_capture.then(|| {
                div()
                    .id("net-before-capture")
                    .test_support()
                    .aria_label("Started before fdt connected")
                    .text_size(font::LABEL)
                    .text_color(Hsla::from(t.text_muted))
                    .child("Started before fdt connected")
            }))
            .into_any_element()
    }

    // ---- bodies ----

    fn body_content(
        &mut self,
        rec: &Arc<HttpRecord>,
        kind: BodyKind,
        connected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = *cx.tokens();
        let pre = prefix(kind);
        let message = |suffix: &str, text: String| {
            div()
                .id(sid(format!("{pre}-{suffix}")))
                .test_support()
                .aria_label(SharedString::from(text.clone()))
                .text_color(Hsla::from(t.text_muted))
                .child(text)
                .into_any_element()
        };
        match rec.body(kind) {
            BodyState::NotCaptured => message("empty", "No body.".to_string()),
            BodyState::Pending => {
                let show = self.pending_skeleton(rec.id, kind, cx);
                div()
                    .h(row::LOG)
                    .children(show.then(|| {
                        div()
                            .id(sid(format!("{pre}-skeleton")))
                            .test_support()
                            .child(Skeleton::new().h(row::LOG))
                    }))
                    .into_any_element()
            }
            BodyState::TooLarge { len } => div()
                .flex()
                .flex_col()
                .gap(space::S6)
                .child(message(
                    "too-large",
                    format!(
                        "This body wasn't copied automatically ({}).",
                        format_bytes(*len)
                    ),
                ))
                .child(self.load_button(rec.id, kind, connected, Some(*len), cx))
                .into_any_element(),
            BodyState::Evicted { .. } => {
                let budget = settings(cx).body_budget_mib;
                message(
                    "evicted",
                    format!("This body was removed to stay within the {budget} MB body budget."),
                )
            }
            BodyState::Unavailable { reason } => message("unavailable", reason.clone()),
            BodyState::Captured(body) => {
                let body = body.clone();
                self.captured_body(rec, kind, &body, connected, cx)
            }
        }
    }

    fn captured_body(
        &mut self,
        rec: &Arc<HttpRecord>,
        kind: BodyKind,
        body: &Body,
        connected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = *cx.tokens();
        let pre = prefix(kind);
        let mut col = div().flex().flex_col().gap(space::S6);
        if body.truncated() {
            let text = format!(
                "Showing the first {} of {}.",
                format_bytes(body.bytes.len() as u64),
                format_bytes(body.full_len)
            );
            col = col.child(
                div()
                    .id(sid(format!("{pre}-truncated")))
                    .test_support()
                    .aria_label(SharedString::from(text.clone()))
                    .text_color(Hsla::from(t.text_muted))
                    .child(text),
            );
            col = col.child(self.load_button(rec.id, kind, connected, Some(body.full_len), cx));
        }
        let skeleton = match self.details.cache.get(&BodyKey {
            id: rec.id,
            kind,
            bytes: (body.bytes.as_ptr() as usize, body.bytes.len()),
            content_type: match kind {
                BodyKind::Response => rec.content_type(),
                BodyKind::Request => request_content_type(rec),
            }
            .map(str::to_string),
        }) {
            Some(Slot::Rendering { skeleton, .. }) => *skeleton,
            _ => false,
        };
        let Some(rendered) = self.rendered(rec, kind, body, cx) else {
            return col
                .child(div().h(row::LOG).children(skeleton.then(|| {
                    div()
                        .id(sid(format!("{pre}-skeleton")))
                        .test_support()
                        .child(Skeleton::new().h(row::LOG))
                })))
                .into_any_element();
        };
        if let Some(image) = rendered.image.clone() {
            let lines = rendered.lines.clone();
            let note = rendered.note.clone();
            let picture = img(image)
                .id(sid(format!("{pre}-image")))
                .max_w_full()
                .max_h(layout::IMAGE_PREVIEW_MAX)
                // An image that does not decode shows its bytes instead.
                .with_fallback(move || fallback_hex(pre, &lines, note.as_deref(), &t))
                .test_support();
            return col.child(picture).into_any_element();
        }
        if let Some(note) = rendered.note.clone() {
            col = col.child(
                div()
                    .id(sid(format!("{pre}-note")))
                    .test_support()
                    .aria_label(SharedString::from(note.clone()))
                    .text_size(font::LABEL)
                    .text_color(Hsla::from(t.text_muted))
                    .child(note),
            );
        }
        let count = rendered.lines.len();
        if count == 0 {
            return col
                .child(
                    div()
                        .id(sid(format!("{pre}-empty")))
                        .test_support()
                        .aria_label("Empty body.")
                        .text_color(Hsla::from(t.text_muted))
                        .child("Empty body."),
                )
                .into_any_element();
        }
        let lines = rendered.lines.clone();
        let colour = Hsla::from(t.text);
        let list = uniform_list(
            sid(format!("{pre}-lines")),
            count,
            cx.processor(move |_, range: Range<usize>, _w, _cx| {
                range
                    .filter_map(|i| {
                        let line = lines.get(i)?;
                        Some(
                            Self::mono_row()
                                .id(sid(format!("{pre}-line-{i}")))
                                .test_support()
                                .aria_label(SharedString::from(line.clone()))
                                .truncate()
                                .text_color(colour)
                                .child(line.clone())
                                .into_any_element(),
                        )
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.details.scroll[kind_index(kind)])
        .size_full();
        col.child(
            div()
                .h(row::LOG * count.min(BODY_ROWS) as f32)
                .flex_none()
                .child(list),
        )
        .into_any_element()
    }

    fn load_button(
        &self,
        id: EntryId,
        kind: BodyKind,
        connected: bool,
        full_len: Option<u64>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let t = *cx.tokens();
        let button_id = match kind {
            BodyKind::Response => "net-load-full",
            BodyKind::Request => "net-request-load-full",
        };
        let waiting = self.details.loads.get(&(id, kind));
        let mut button = action_button(button_id, "Load full body", &t)
            .flex()
            .items_center()
            .gap(space::S4);
        let hint = load_full_hint(connected)
            .map(str::to_string)
            .or_else(|| load_full_too_big_hint(full_len));
        if let Some(hint) = hint {
            button = button
                .opacity(alpha::DISABLED)
                .tooltip(move |window, cx| Tooltip::new(hint.clone()).build(window, cx));
        } else {
            button =
                button.on_click(cx.listener(move |this, _, _, cx| this.load_full(id, kind, cx)));
        }
        if waiting.is_some_and(|l| l.spinner) {
            button = button.child(
                div()
                    .id(sid(format!("{button_id}-spinner")))
                    .test_support()
                    .child(Spinner::new().with_size(Size::XSmall)),
            );
        }
        button.into_any_element()
    }
}

/// Whether a body is one "Load full body" can improve.
fn offers_full(state: &BodyState) -> bool {
    match state {
        BodyState::TooLarge { .. } => true,
        BodyState::Captured(b) => b.truncated(),
        _ => false,
    }
}

/// The first lines of an image's bytes, as a plain column.
fn fallback_hex(
    pre: &'static str,
    lines: &[String],
    note: Option<&str>,
    t: &fdt_ui::tokens::Palette,
) -> AnyElement {
    div()
        .id(sid(format!("{pre}-image-fallback")))
        .test_support()
        .flex()
        .flex_col()
        .child(
            div()
                .text_size(font::LABEL)
                .text_color(Hsla::from(t.text_muted))
                .child(format!(
                    "This image can't be shown. {}",
                    note.unwrap_or_default()
                )),
        )
        .children(
            lines
                .iter()
                .take(FALLBACK_LINES)
                .enumerate()
                .map(|(i, line)| {
                    NetworkView::mono_row()
                        .id(sid(format!("{pre}-line-{i}")))
                        .test_support()
                        .aria_label(SharedString::from(line.clone()))
                        .truncate()
                        .child(line.clone())
                }),
        )
        .into_any_element()
}

fn action_button(
    id: &'static str,
    label: &'static str,
    t: &fdt_ui::tokens::Palette,
) -> impl IntoElement + gpui_kit::StatefulInteractiveElement + gpui_kit::Styled + gpui_kit::ParentElement
{
    div()
        .id(id)
        .test_support()
        .aria_label(label)
        .flex_none()
        .px(space::S8)
        .py(space::S2)
        .rounded(radius::CONTROL)
        .bg(Hsla::from(t.elevated))
        .text_size(font::LABEL)
        .text_color(Hsla::from(t.text))
        .cursor_pointer()
        .child(label)
}

fn meta(
    id: &'static str,
    key: &'static str,
    value: String,
    t: &fdt_ui::tokens::Palette,
) -> AnyElement {
    div()
        .flex()
        .gap(space::S8)
        .child(
            div()
                .flex_none()
                .w(fdt_ui::tokens::table::COL_METHOD + space::S8)
                .text_color(Hsla::from(t.text_muted))
                .child(key),
        )
        .child(
            div()
                .id(id)
                .test_support()
                .aria_label(SharedString::from(value.clone()))
                .min_w_0()
                .child(value),
        )
        .into_any_element()
}

fn headers_body(
    id: &'static str,
    headers: &[(String, Vec<String>)],
    cx: &mut Context<NetworkView>,
) -> AnyElement {
    let t = *cx.tokens();
    if headers.is_empty() {
        return div()
            .id(sid(format!("{id}-empty")))
            .test_support()
            .aria_label("No headers.")
            .text_color(Hsla::from(t.text_muted))
            .child("No headers.")
            .into_any_element();
    }
    let sorted = sorted_headers(headers);
    let more = sorted.len().saturating_sub(HEADERS_MAX);
    div()
        .flex()
        .flex_col()
        .gap(space::S2)
        .children(
            sorted
                .into_iter()
                .take(HEADERS_MAX)
                .enumerate()
                .map(|(i, (k, v))| {
                    div()
                        .id(sid(format!("{id}-{i}")))
                        .test_support()
                        .aria_label(SharedString::from(format!("{k}: {v}")))
                        .font_family(font::MONO)
                        .text_size(font::DENSE)
                        .whitespace_normal()
                        .child(
                            div()
                                .text_color(Hsla::from(t.text_muted))
                                .child(format!("{k}: ")),
                        )
                        .child(v)
                }),
        )
        .children((more > 0).then(|| {
            div()
                .text_size(font::LABEL)
                .text_color(Hsla::from(t.text_muted))
                .child(format!("{more} more. Copy headers has them all."))
        }))
        .into_any_element()
}

fn timing_body(rec: &HttpRecord, cx: &mut Context<NetworkView>) -> AnyElement {
    let t = *cx.tokens();
    let mut rows: Vec<(String, u64)> = rec
        .events
        .iter()
        .map(|(name, at)| (name.clone(), at.0.saturating_sub(rec.start.0)))
        .collect();
    if rows.is_empty() {
        for (name, at) in [
            ("Request sent", rec.request_sent),
            ("Response started", rec.response_start),
        ] {
            if let Some(at) = at {
                rows.push((name.to_string(), at.0.saturating_sub(rec.start.0)));
            }
        }
    }
    let total = rec
        .duration_ns()
        .map_or_else(|| "Pending".to_string(), format_duration_ns);
    div()
        .flex()
        .flex_col()
        .gap(space::S2)
        .children(rows.into_iter().enumerate().map(|(i, (name, ns))| {
            let text = format!("{name}: {}", offset_text(ns));
            div()
                .id(sid(format!("net-timing-{i}")))
                .test_support()
                .aria_label(SharedString::from(text.clone()))
                .child(text)
        }))
        .child(
            div()
                .id("net-timing-total")
                .test_support()
                .aria_label(SharedString::from(format!("Total: {total}")))
                .font_weight(font::SEMIBOLD)
                .text_color(Hsla::from(t.text))
                .child(format!("Total: {total}")),
        )
        .into_any_element()
}

fn error_body(rec: &HttpRecord, cx: &mut Context<NetworkView>) -> AnyElement {
    let t = *cx.tokens();
    let error = rec.error.clone().unwrap_or_default();
    div()
        .id("net-error")
        .test_support()
        .aria_label(SharedString::from(error.clone()))
        .font_family(font::MONO)
        .text_size(font::DENSE)
        .whitespace_normal()
        .text_color(Hsla::from(t.danger))
        .child(error)
        .into_any_element()
}

#[cfg(test)]
mod preview_tests {
    use super::*;

    fn png_of(len: usize) -> Body {
        let mut bytes = crate::test_support::PNG.to_vec();
        bytes.resize(len, 0);
        Body {
            full_len: len as u64,
            bytes: bytes.into(),
        }
    }

    #[test]
    fn an_image_over_8_mib_shows_hex_and_a_note_without_decoding() {
        let r = build(Some("image/png"), &png_of(9 << 20));
        assert!(r.image.is_none(), "no copy or decode of a 9 MiB image");
        assert_eq!(r.note.as_deref(), Some("Image too large to preview."));
        assert!(r.lines[0].starts_with("00000000  89 50 4e 47"));
        let ok = build(Some("image/png"), &png_of(1 << 20));
        assert!(ok.image.is_some());
    }
}
