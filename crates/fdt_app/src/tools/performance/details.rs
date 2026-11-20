//! The frame details pane: the phase breakdown, the one-sentence explanation, the most rebuilt
//! widgets, the busiest functions and the requests and log lines around the frame.
//!
//! `fdt_analysis::explain` runs in `cx.background_spawn`. Its result is kept for the selected
//! frame, keyed by the session and the frame's id, and recomputed when the frame's phases or its
//! CPU coverage change.

use super::view::PerformanceView;
use crate::nav::{Location, Page, Selection, Tool};
use crate::tools::network::model::{format_duration_ns, format_time, status_text};
use crate::tools::{OpenLocation, ReplaceLocation};
use fdt_analysis::breakdown::Phase;
use fdt_analysis::explain::{CpuState, Explanation, explain};
use fdt_analysis::user_code::is_user_library;
use fdt_capture::ConnStatus;
use fdt_store::Snapshot;
use fdt_store::cpu::CpuStatus;
use fdt_store::frames::{FrameStatus, RasterPhase, UiPhases, frame_status};
use fdt_store::time::{EntryId, SessionTime};
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{Palette, alpha, font, layout, motion, radius, row, space};
use gpui_kit::component::resizable::{
    ResizablePanelEvent, ResizableState, h_resizable, resizable_panel,
};
use gpui_kit::component::skeleton::Skeleton;
use gpui_kit::{
    AnyElement, AppContext as _, Context, Div, Entity, Hsla, InteractiveElement, IntoElement,
    ParentElement, Pixels, SharedString, StatefulInteractiveElement, Styled, Subscription, Task,
    TestSupportExt as _, div, relative,
};
use std::sync::Arc;

const MAX_NAME_CHARS: usize = 80;
const TOP: usize = 10;
const DASH: &str = "\u{2014}";
const SEP: &str = " \u{b7} ";

/// A name cut to 80 characters: the first 79 and an ellipsis.
pub(super) fn elide(name: &str) -> String {
    if name.chars().nth(MAX_NAME_CHARS).is_none() {
        return name.to_string();
    }
    let mut s: String = name.chars().take(MAX_NAME_CHARS - 1).collect();
    s.push('\u{2026}');
    s
}

fn phase_label(phase: Phase) -> &'static str {
    match phase {
        Phase::Build => "Build",
        Phase::Layout => "Layout",
        Phase::Paint => "Paint",
        Phase::Composite => "Composite",
        Phase::Finalize => "Finalize",
        Phase::OtherUi => "Other UI",
        Phase::Raster => "Raster",
    }
}

/// `24.2 ms`: one decimal, so a column of phases lines up.
fn ms1(us: u32) -> String {
    format!("{:.1} ms", f64::from(us) / 1000.0)
}

fn status_label(status: FrameStatus) -> &'static str {
    match status {
        FrameStatus::Ok => "OK",
        FrameStatus::Slow => "Slow",
        FrameStatus::Janky => "Janky",
    }
}

/// What the pane's content depends on besides the frame's identity.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Fingerprint {
    elapsed_us: u32,
    build_us: u32,
    raster_us: u32,
    ui: Option<UiPhases>,
    raster: Option<RasterPhase>,
    budget_us: u32,
    coverage: Option<SessionTime>,
    cpu_status: CpuStatus,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Key {
    /// Run ids restart in every session, so the session is part of the key.
    session: u64,
    id: EntryId,
    fingerprint: Fingerprint,
}

/// The details pane's state.
pub(super) struct DetailsState {
    key: Option<Key>,
    result: Option<Arc<Explanation>>,
    /// Set once the analysis has taken longer than `LOADING_DELAY`.
    skeleton: bool,
    /// How many times the analysis was started.
    runs: usize,
    job: Option<Task<()>>,
    timer: Option<Task<()>>,
    split: Option<Entity<ResizableState>>,
    width: Pixels,
    split_sub: Option<Subscription>,
}

impl Default for DetailsState {
    fn default() -> Self {
        Self {
            key: None,
            result: None,
            skeleton: false,
            runs: 0,
            job: None,
            timer: None,
            split: None,
            width: layout::DETAILS_WIDTH,
            split_sub: None,
        }
    }
}

impl DetailsState {
    fn clear(&mut self) {
        self.key = None;
        self.result = None;
        self.skeleton = false;
        self.job = None;
        self.timer = None;
    }

    pub(super) fn is_open(&self) -> bool {
        self.key.is_some()
    }
}

fn heading(
    t: &Palette,
    id: &'static str,
    title: &'static str,
) -> impl IntoElement + Styled + ParentElement + StatefulInteractiveElement {
    div()
        .id(id)
        .test_support()
        .aria_label(title)
        .text_size(font::LABEL)
        .font_weight(font::SEMIBOLD)
        .text_color(Hsla::from(t.text_muted))
        .child(title)
}

fn note(
    t: &Palette,
    id: &'static str,
    text: &str,
) -> impl IntoElement + Styled + ParentElement + StatefulInteractiveElement {
    div()
        .id(id)
        .test_support()
        .aria_label(SharedString::from(text.to_string()))
        .text_color(Hsla::from(t.text_muted))
        .child(text.to_string())
}

fn button(
    t: &Palette,
    id: &'static str,
    label: &'static str,
) -> impl IntoElement + Styled + ParentElement + StatefulInteractiveElement {
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

fn chip(
    id: SharedString,
    text: &'static str,
    colour: Hsla,
) -> impl IntoElement + Styled + ParentElement + StatefulInteractiveElement {
    div()
        .id(id)
        .test_support()
        .aria_label(text)
        .flex_none()
        .px(space::S8)
        .py(space::S2)
        .rounded(radius::CHIP)
        .bg(colour.opacity(f32::from(alpha::ACTIVE_ROW) / 255.0))
        .text_size(font::LABEL)
        .text_color(colour)
        .child(text)
}

impl PerformanceView {
    #[cfg(test)]
    pub(crate) fn details_runs(&self) -> usize {
        self.details.runs
    }

    /// Opens, closes or refreshes the pane to match the selected frame and the latest snapshot.
    /// Starts the analysis only when the frame, its phases or its CPU coverage changed.
    pub(super) fn sync_details(&mut self, cx: &mut Context<Self>) {
        let snap = self.session.read(cx).snapshot().clone();
        let frame = self
            .selection
            .and_then(|id| snap.frames.get(id.seq as usize))
            .filter(|f| Some(f.id) == self.selection);
        let Some(frame) = frame else {
            self.details.clear();
            return;
        };
        let budget_us = snap.frame_budget_us(frame.run);
        let key = Key {
            session: snap.session,
            id: frame.id,
            fingerprint: Fingerprint {
                elapsed_us: frame.elapsed_us,
                build_us: frame.build_us,
                raster_us: frame.raster_us,
                ui: frame.ui,
                raster: frame.raster,
                budget_us,
                coverage: snap.cpu.coverage_end(frame.run),
                cpu_status: snap.cpu.status(frame.run).clone(),
            },
        };
        let same_frame = self
            .details
            .key
            .as_ref()
            .is_some_and(|k| k.session == key.session && k.id == key.id);
        if self.details.key.as_ref() == Some(&key) {
            return;
        }
        if !same_frame {
            // Another frame: the old result is not this one's, and no skeleton shows until the
            // analysis has been running for `LOADING_DELAY`.
            self.details.result = None;
            self.details.skeleton = false;
            self.details.timer = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(motion::LOADING_DELAY).await;
                this.update(cx, |this, cx| {
                    if this.details.result.is_none() && this.details.key.is_some() {
                        this.details.skeleton = true;
                        cx.notify();
                    }
                })
                .ok();
            }));
        }
        self.details.runs += 1;
        let seq = frame.id.seq as usize;
        self.details.key = Some(key.clone());
        self.details.job = Some(cx.spawn(async move |this, cx| {
            let explanation = cx
                .background_spawn(async move { explain(&snap, seq) })
                .await;
            this.update(cx, |this, cx| {
                // A newer selection or snapshot replaced this job's key.
                if this.details.key.as_ref() == Some(&key) {
                    this.details.result = Some(Arc::new(explanation));
                    this.details.skeleton = false;
                    this.details.timer = None;
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// Closes the pane: the frame is deselected and the range stays.
    fn close_details(&mut self, cx: &mut Context<Self>) {
        cx.emit(ReplaceLocation(Location {
            page: Page::Tool(Tool::Performance),
            selection: None,
            range: self.range,
        }));
    }

    fn open_related(&mut self, tool: Tool, selection: Selection, cx: &mut Context<Self>) {
        cx.emit(OpenLocation(Location {
            page: Page::Tool(tool),
            selection: Some(selection),
            range: self.range,
        }));
    }

    /// `main` beside the details pane in a split the user can resize; `main` alone while no
    /// frame is selected.
    pub(super) fn with_details(&mut self, main: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        if !self.details.is_open() {
            return main;
        }
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
        div()
            .flex_1()
            .min_h_0()
            .w_full()
            .child(
                h_resizable("perf-split")
                    .with_state(&state)
                    .child(resizable_panel().child(div().size_full().flex().flex_col().child(main)))
                    .child(
                        resizable_panel()
                            .size(self.details.width)
                            .size_range(layout::DETAILS_MIN..layout::DETAILS_MAX)
                            .flex_none()
                            .child(pane),
                    ),
            )
            .into_any_element()
    }

    fn render_details(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let t = *cx.tokens();
        let snap = self.session.read(cx).snapshot().clone();
        let connected = matches!(self.session.read(cx).status(), ConnStatus::Connected { .. });
        let frame = self
            .selection
            .and_then(|id| snap.frames.get(id.seq as usize))
            .cloned();
        let shell = div()
            .id("frame-details")
            .test_support()
            .size_full()
            .flex()
            .flex_col()
            .border_l(layout::BORDER)
            .border_color(Hsla::from(t.border))
            .bg(Hsla::from(t.surface))
            .text_size(font::BODY)
            .text_color(Hsla::from(t.text));
        let Some(frame) = frame else {
            return shell.into_any_element();
        };
        let status = frame_status(frame.time_us(), snap.frame_budget_us(frame.run));
        let status_colour = Hsla::from(match status {
            FrameStatus::Ok => t.success,
            FrameStatus::Slow => t.warning,
            FrameStatus::Janky => t.danger,
        });
        let title = format!("Frame #{}", frame.number);
        let header = div()
            .flex()
            .flex_none()
            .items_center()
            .gap(space::S8)
            .p(space::S12)
            .child(
                div()
                    .id("details-title")
                    .test_support()
                    .aria_label(SharedString::from(title.clone()))
                    .flex_1()
                    .font_weight(font::SEMIBOLD)
                    .child(title),
            )
            .child(chip(
                SharedString::from("details-status"),
                status_label(status),
                status_colour,
            ))
            .child(
                button(&t, "details-close", "Close")
                    .on_click(cx.listener(|this, _, _, cx| this.close_details(cx))),
            );
        let body = div()
            .id("details-body")
            .test_support()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap(space::S12)
            .px(space::S12)
            .pb(space::S12);
        let content: Vec<AnyElement> = match self.details.result.clone() {
            Some(e) => self.render_sections(&e, &snap, connected, cx),
            None => self
                .details
                .skeleton
                .then(|| {
                    div()
                        .id("details-skeleton")
                        .test_support()
                        .flex()
                        .flex_col()
                        .gap(space::S8)
                        .child(Skeleton::new().h(layout::PHASE_BAR_HEIGHT))
                        .child(Skeleton::new().h(row::LOG))
                        .child(Skeleton::new().h(row::LOG))
                        .into_any_element()
                })
                .into_iter()
                .collect(),
        };
        shell
            .child(header)
            .child(body.children(content))
            .into_any_element()
    }

    fn render_sections(
        &mut self,
        e: &Explanation,
        snap: &Snapshot,
        connected: bool,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let t = *cx.tokens();
        vec![
            self.breakdown_section(e, &t).into_any_element(),
            div()
                .id("details-sentence")
                .test_support()
                .aria_label(SharedString::from(e.sentence.clone()))
                .text_size(font::BODY)
                .font_weight(font::MEDIUM)
                .child(e.sentence.clone())
                .into_any_element(),
            self.rebuilds_section(e, &t, connected, cx)
                .into_any_element(),
            self.functions_section(e, snap, &t, connected, cx)
                .into_any_element(),
            self.related_section(e, snap, &t, cx).into_any_element(),
        ]
    }

    fn breakdown_section(&self, e: &Explanation, t: &Palette) -> Div {
        let bd = &e.breakdown;
        let total: u64 = bd.segments.iter().map(|s| u64::from(s.us)).sum();
        let mut ui_index = 0;
        let mut bar = div()
            .id("details-bar")
            .test_support()
            .flex()
            .w_full()
            .h(layout::PHASE_BAR_HEIGHT + space::S2);
        let mut entries: Vec<(Phase, String)> = Vec::new();
        for seg in &bd.segments {
            let over = bd.over == Some(seg.phase);
            let text = if over {
                format!("{} {}, over budget", phase_label(seg.phase), ms1(seg.us))
            } else {
                format!("{} {}", phase_label(seg.phase), ms1(seg.us))
            };
            entries.push((seg.phase, text));
            let colour = if seg.phase == Phase::Raster {
                Hsla::from(t.lane_network)
            } else {
                let c = t
                    .accent
                    .with_alpha(alpha::PHASE[ui_index.min(alpha::PHASE.len() - 1)]);
                ui_index += 1;
                c
            };
            if seg.us == 0 || total == 0 {
                continue;
            }
            let share = seg.us as f32 / total as f32;
            bar = bar.child(
                div()
                    .w(relative(share))
                    .flex_none()
                    .flex()
                    .flex_col()
                    .child(div().w_full().h(layout::PHASE_BAR_HEIGHT).bg(colour))
                    .children(over.then(|| {
                        div()
                            .id("details-over-mark")
                            .test_support()
                            .w_full()
                            .h(space::S2)
                            .bg(Hsla::from(t.danger))
                    })),
            );
        }
        let legend_text = entries
            .iter()
            .map(|(_, s)| s.as_str())
            .collect::<Vec<_>>()
            .join(SEP);
        let mut legend = div()
            .id("details-legend")
            .test_support()
            .aria_label(SharedString::from(legend_text))
            .flex()
            .flex_wrap()
            .text_size(font::DENSE)
            .text_color(Hsla::from(t.text_muted));
        for (i, (phase, text)) in entries.into_iter().enumerate() {
            if i > 0 {
                legend = legend.child(SEP);
            }
            legend = if bd.over == Some(phase) {
                legend.child(
                    div()
                        .id("details-over")
                        .test_support()
                        .aria_label(SharedString::from(text.clone()))
                        .text_color(Hsla::from(t.danger))
                        .child(text),
                )
            } else {
                legend.child(text)
            };
        }
        div()
            .flex()
            .flex_col()
            .gap(space::S4)
            .child(bar)
            .child(legend)
            .children((!bd.has_timeline).then(|| {
                note(
                    t,
                    "details-no-timeline",
                    "No timeline detail for this frame.",
                )
                .text_size(font::DENSE)
            }))
    }

    fn rebuilds_section(
        &mut self,
        e: &Explanation,
        t: &Palette,
        connected: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let section = div().flex().flex_col().gap(space::S4).child(heading(
            t,
            "details-section-rebuilds",
            "Most rebuilt widgets",
        ));
        let Some(rebuilds) = &e.rebuilds else {
            return section
                .child(note(
                    t,
                    "details-rebuilds-note",
                    "Turn on Trace widget builds to see which widgets rebuilt.",
                ))
                .children(connected.then(|| {
                    div().flex().child(
                        button(t, "details-trace-builds", "Trace widget builds").on_click(
                            cx.listener(|this, _, _, cx| {
                                if let Some(c) = this.session.read(cx).control() {
                                    c.set_trace_widget_builds(true);
                                }
                            }),
                        ),
                    )
                }));
        };
        let mut section = section;
        for (i, w) in rebuilds.widgets.iter().take(TOP).enumerate() {
            let name = elide(&w.name);
            let builds = format!(
                "{} {}",
                w.builds,
                if w.builds == 1 { "build" } else { "builds" }
            );
            let own = format!("self {}", format_duration_ns(w.self_ns));
            section = section.child(
                div()
                    .id(SharedString::from(format!("details-widget-{i}")))
                    .test_support()
                    .aria_label(SharedString::from(format!("{name}{SEP}{builds}{SEP}{own}")))
                    .flex()
                    .gap(space::S8)
                    .child(
                        div()
                            .id(SharedString::from(format!("details-widget-{i}-name")))
                            .test_support()
                            .aria_label(SharedString::from(name.clone()))
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(font::MONO)
                            .text_size(font::DENSE)
                            .child(name),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(Hsla::from(t.text_muted))
                            .child(builds),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_color(Hsla::from(t.text_muted))
                            .child(own),
                    ),
            );
        }
        section
    }

    fn functions_section(
        &mut self,
        e: &Explanation,
        snap: &Snapshot,
        t: &Palette,
        connected: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut section = div().flex().flex_col().gap(space::S4).child(heading(
            t,
            "details-section-functions",
            "Busiest functions",
        ));
        let covered = matches!(e.cpu, CpuState::Covered) && !e.functions.is_empty();
        if !covered {
            return match &e.cpu {
                CpuState::Pending if connected => section
                    .child(note(
                        t,
                        "details-cpu-note",
                        "CPU samples for this frame haven't been fetched yet.",
                    ))
                    .child(div().flex().child(
                        button(t, "details-fetch-now", "Fetch now").on_click(cx.listener(
                            |this, _, _, cx| {
                                if let Some(c) = this.session.read(cx).control() {
                                    c.fetch_cpu_samples();
                                }
                            },
                        )),
                    )),
                CpuState::Pending => section.child(note(
                    t,
                    "details-cpu-note",
                    "CPU samples for this frame weren't fetched before the app disconnected.",
                )),
                CpuState::Off(reason) => section.child(note(t, "details-cpu-note", reason)),
                CpuState::Covered | CpuState::NoSamples => section.child(note(
                    t,
                    "details-cpu-note",
                    "No CPU samples were taken during this frame.",
                )),
            };
        }
        for (i, f) in e.functions.iter().take(TOP).enumerate() {
            let info = snap.cpu.function(f.function);
            let name = elide(&info.name);
            let user = is_user_library(info.library.as_deref());
            let counts = format!("{} self{SEP}{} total", f.self_count, f.total_count);
            section = section.child(
                div()
                    .id(SharedString::from(format!("details-function-{i}")))
                    .test_support()
                    .aria_label(SharedString::from(format!("{name}{SEP}{counts}")))
                    .flex()
                    .items_center()
                    .gap(space::S8)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(font::MONO)
                            .text_size(font::DENSE)
                            .child(name),
                    )
                    .children(user.then(|| {
                        chip(
                            SharedString::from(format!("details-function-{i}-user")),
                            "your code",
                            Hsla::from(t.accent),
                        )
                    }))
                    .child(
                        div()
                            .flex_none()
                            .text_color(Hsla::from(t.text_muted))
                            .child(counts),
                    ),
            );
        }
        section
    }

    fn related_section(
        &mut self,
        e: &Explanation,
        snap: &Snapshot,
        t: &Palette,
        cx: &mut Context<Self>,
    ) -> Div {
        let mut section = div().flex().flex_col().gap(space::S4).child(heading(
            t,
            "details-section-related",
            "Related",
        ));
        let row = |id: String, text: String| {
            div()
                .id(SharedString::from(id))
                .test_support()
                .aria_label(SharedString::from(text.clone()))
                .w_full()
                .truncate()
                .px(space::S4)
                .rounded(radius::CONTROL)
                .cursor_pointer()
                .hover(|s| s.bg(Hsla::from(t.elevated)))
                .child(text)
        };
        let mut any = false;
        for (i, &seq) in e.related.requests.iter().enumerate() {
            let Some(r) = snap.net.get(seq as usize) else {
                continue;
            };
            any = true;
            let id = r.id;
            let duration = r.duration_ns().map_or(DASH.to_string(), format_duration_ns);
            let text = format!(
                "{} {}{SEP}{}{SEP}{}",
                r.method,
                r.path,
                status_text(r),
                duration
            );
            section = section.child(row(format!("details-request-{i}"), text).on_click(
                cx.listener(move |this, _, _, cx| {
                    this.open_related(Tool::Network, Selection::Request(id), cx)
                }),
            ));
        }
        for (i, &seq) in e.related.logs.iter().enumerate() {
            let Some(r) = snap.logs.get_row(seq as usize) else {
                continue;
            };
            any = true;
            let id = r.id;
            let line = elide(r.text.lines().next().unwrap_or(""));
            let text = format!(
                "{}{SEP}{}{SEP}{}",
                format_time(r.time),
                r.level.label(),
                line
            );
            section = section.child(row(format!("details-log-{i}"), text).on_click(cx.listener(
                move |this, _, _, cx| this.open_related(Tool::Logs, Selection::Log(id), cx),
            )));
        }
        if !any {
            section = section.child(note(
                t,
                "details-related-empty",
                "No requests or log lines around this frame.",
            ));
        }
        section
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::ShowPerformance;
    use crate::nav::{Location, Page, Selection, TimeRange, Tool};
    use crate::shell::MainWindow;
    use crate::test_support::{
        connected, frame_at, log_at, open_shell_fake, open_shell_snapshot, request, snapshot,
        writer,
    };
    use fdt_store::SessionWriter;
    use fdt_store::frames::UiPhases;
    use fdt_store::logs::LogLevel;
    use fdt_store::time::{RunId, SessionTime};
    use fdt_store::timeline::{Slice, SliceCat, TrackKind};
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{Entity, TestAppContext, VisualTestContext};
    use std::sync::Arc;

    const MS: u64 = 1_000_000;

    fn label(cx: &mut VisualTestContext, id: &'static str) -> Option<String> {
        cx.update(|window, cx| {
            window.render_frame(cx);
            window
                .try_find(id)
                .and_then(|e| e.label().map(str::to_string))
        })
    }

    fn exists(cx: &mut VisualTestContext, id: &'static str) -> bool {
        crate::test_support::exists(cx, id)
    }

    fn click(cx: &mut VisualTestContext, id: &'static str) {
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click(id, cx);
        });
        cx.run_until_parked();
    }

    fn location(
        shell: &Entity<MainWindow>,
        cx: &mut VisualTestContext,
        seq: usize,
        range: Option<TimeRange>,
    ) -> Location {
        let id = shell.read_with(cx, |s, cx| {
            s.performance_view()
                .read(cx)
                .snapshot_frame(seq, cx)
                .expect("frame")
                .id
        });
        Location {
            page: Page::Tool(Tool::Performance),
            selection: Some(Selection::Frame(id)),
            range,
        }
    }

    fn select(shell: &Entity<MainWindow>, cx: &mut VisualTestContext, seq: usize) {
        let to = location(shell, cx, seq, None);
        shell.update_in(cx, |s, w, cx| s.navigate(to, w, cx));
        cx.run_until_parked();
        cx.update(|window, cx| window.render_frame(cx));
    }

    /// A frame at `start_ms` with a timeline: 30 ms of UI work split into phases.
    fn phased_frame(w: &mut SessionWriter, run: RunId, number: u64, start_ms: u64, raster_ms: u64) {
        w.push_frame(frame_at(run, number, start_ms, 30, raster_ms));
        w.set_frame_ui(
            run,
            number,
            UiPhases {
                begin: SessionTime(start_ms * MS),
                ui_us: 30_000,
                build_us: 24_200,
                layout_us: 0,
                paint_us: 2_000,
                composite_us: 1_800,
                finalize_us: 2_000,
                widgets_built: None,
            },
        );
    }

    /// A frame whose BUILD holds one widget named `name`, built `times` times.
    fn widget_frame(w: &mut SessionWriter, run: RunId, number: u64, name: &str, times: u64) {
        phased_frame(w, run, number, 100, 2);
        let ui = w.timeline_track(run, TrackKind::Thread, 1, "io.flutter.ui");
        w.set_track_order(ui, 0);
        let begin = 100 * MS;
        let mut put = |depth: u16, name: &str, at_us: u64, dur_us: u64| {
            let s = Slice {
                start: SessionTime(begin + at_us * 1000),
                dur_ns: dur_us * 1000,
                name: w.intern_name(name),
                cat: SliceCat::Dart,
            };
            assert!(w.push_slice(ui, depth, s, depth > 2), "{name} stored");
        };
        put(0, "VsyncProcessCallback", 0, 30_000);
        put(1, "Animator::BeginFrame", 0, 30_000);
        put(2, "BUILD", 0, 24_000);
        for i in 0..times {
            put(3, name, i * 100, 90);
        }
    }

    fn phase_session() -> Arc<fdt_store::Snapshot> {
        snapshot(|w, run| {
            phased_frame(w, run, 0, 100, 2);
            w.push_frame(frame_at(run, 1, 140, 5, 5));
        })
    }

    fn open_perf(
        cx: &mut TestAppContext,
        snap: Arc<fdt_store::Snapshot>,
    ) -> (Entity<MainWindow>, &mut VisualTestContext) {
        let (shell, cx) = open_shell_snapshot(cx, snap);
        cx.dispatch_action(ShowPerformance);
        cx.run_until_parked();
        (shell, cx)
    }

    #[gpui_kit::test]
    fn selecting_a_janky_frame_shows_its_sentence(cx: &mut TestAppContext) {
        let (mut w, run) = writer();
        fdt_store::synth::perf(&mut w, run, 600, 42, 120);
        w.set_refresh_rate(run, 120.0);
        let snap = w.publish();
        let budget = snap.frame_budget_us(run);
        let seq = (0..snap.frames.len())
            .find(|&i| snap.frames.get(i).unwrap().time_us() > 2 * budget)
            .expect("a janky frame");
        let want = fdt_analysis::explain::explain(&snap, seq).sentence;
        assert!(!want.is_empty());
        let (shell, cx) = open_perf(cx, snap.clone());
        assert!(!exists(cx, "frame-details"));
        select(&shell, cx, seq);
        assert!(exists(cx, "frame-details"));
        assert_eq!(label(cx, "details-sentence"), Some(want));
        let number = snap.frames.get(seq).unwrap().number;
        assert_eq!(label(cx, "details-title"), Some(format!("Frame #{number}")));
        assert_eq!(label(cx, "details-status").as_deref(), Some("Janky"));
        click(cx, "details-close");
        assert!(!exists(cx, "frame-details"));
        assert_eq!(shell.read_with(cx, |s, _| s.current().selection), None);
    }

    #[gpui_kit::test]
    fn over_phase_is_labelled_in_text(cx: &mut TestAppContext) {
        let (shell, cx) = open_perf(cx, phase_session());
        select(&shell, cx, 0);
        assert_eq!(
            label(cx, "details-over").as_deref(),
            Some("Build 24.2 ms, over budget")
        );
        assert_eq!(
            label(cx, "details-legend").as_deref(),
            Some(
                "Build 24.2 ms, over budget \u{b7} Layout 0.0 ms \u{b7} Paint 2.0 ms \u{b7} Composite 1.8 ms \u{b7} Finalize 2.0 ms \u{b7} Raster 2.0 ms"
            )
        );
        assert!(exists(cx, "details-over-mark"));
        assert!(!exists(cx, "details-no-timeline"));
        // A frame within its budget has no over phase.
        select(&shell, cx, 1);
        assert!(!exists(cx, "details-over"));
        assert!(!exists(cx, "details-over-mark"));
    }

    #[gpui_kit::test]
    fn frame_without_timeline_says_so(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| {
            w.push_frame(frame_at(run, 0, 100, 40, 3));
        });
        let (shell, cx) = open_perf(cx, snap);
        select(&shell, cx, 0);
        assert_eq!(
            label(cx, "details-no-timeline").as_deref(),
            Some("No timeline detail for this frame.")
        );
        assert!(
            label(cx, "details-legend")
                .unwrap()
                .starts_with("Build 40.0 ms")
        );
    }

    #[gpui_kit::test]
    fn pending_cpu_offers_fetch_now(cx: &mut TestAppContext) {
        let (shell, cx, fake) = open_shell_fake(cx);
        fake.snaps.send(phase_session()).unwrap();
        fake.status.send(connected("demo", true)).unwrap();
        cx.run_until_parked();
        cx.dispatch_action(ShowPerformance);
        select(&shell, cx, 0);
        assert_eq!(
            label(cx, "details-cpu-note").as_deref(),
            Some("CPU samples for this frame haven't been fetched yet.")
        );
        click(cx, "details-fetch-now");
        assert_eq!(fake.commands(), vec!["fetch_cpu".to_string()]);
    }

    #[gpui_kit::test]
    fn pending_cpu_without_connection_explains_why(cx: &mut TestAppContext) {
        let (shell, cx) = open_perf(cx, phase_session());
        select(&shell, cx, 0);
        assert_eq!(
            label(cx, "details-cpu-note").as_deref(),
            Some("CPU samples for this frame weren't fetched before the app disconnected.")
        );
        assert!(!exists(cx, "details-fetch-now"));
    }

    #[gpui_kit::test]
    fn cpu_states_without_samples_say_so(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| {
            phased_frame(w, run, 0, 100, 2);
            w.set_cpu_coverage(run, SessionTime(10_000 * MS));
        });
        let (shell, cx) = open_perf(cx, snap);
        select(&shell, cx, 0);
        assert_eq!(
            label(cx, "details-cpu-note").as_deref(),
            Some("No CPU samples were taken during this frame.")
        );
    }

    #[gpui_kit::test]
    fn cpu_off_shows_the_reason(cx: &mut TestAppContext) {
        use fdt_store::cpu::CpuStatus;
        let snap = snapshot(|w, run| {
            phased_frame(w, run, 0, 100, 2);
            w.set_cpu_status(run, CpuStatus::Off("profile mode".into()));
        });
        let (shell, cx) = open_perf(cx, snap);
        select(&shell, cx, 0);
        assert_eq!(
            label(cx, "details-cpu-note").as_deref(),
            Some("profile mode")
        );
    }

    #[gpui_kit::test]
    fn rebuilds_button_turns_tracing_on(cx: &mut TestAppContext) {
        let (shell, cx, fake) = open_shell_fake(cx);
        fake.snaps.send(phase_session()).unwrap();
        fake.status.send(connected("demo", true)).unwrap();
        cx.run_until_parked();
        cx.dispatch_action(ShowPerformance);
        select(&shell, cx, 0);
        assert_eq!(
            label(cx, "details-rebuilds-note").as_deref(),
            Some("Turn on Trace widget builds to see which widgets rebuilt.")
        );
        click(cx, "details-trace-builds");
        assert_eq!(fake.commands(), vec!["trace_builds true".to_string()]);
    }

    #[gpui_kit::test]
    fn rebuilt_widgets_list_names_and_builds(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| widget_frame(w, run, 0, "ProductTile", 5));
        let (shell, cx) = open_perf(cx, snap);
        select(&shell, cx, 0);
        assert!(!exists(cx, "details-rebuilds-note"));
        let row = label(cx, "details-widget-0").expect("a widget row");
        assert!(row.starts_with("ProductTile"), "{row}");
        assert!(row.contains('5'), "{row}");
    }

    #[gpui_kit::test]
    fn long_widget_names_elide(cx: &mut TestAppContext) {
        let long = "W".repeat(200);
        let snap = snapshot(|w, run| widget_frame(w, run, 0, &long, 3));
        let (shell, cx) = open_perf(cx, snap);
        select(&shell, cx, 0);
        let name = label(cx, "details-widget-0-name").expect("a widget row");
        assert_eq!(name.chars().count(), 80, "{name}");
        assert!(name.ends_with('\u{2026}'));
        assert_eq!(elide("short"), "short");
        assert_eq!(elide(&"\u{e9}".repeat(81)).chars().count(), 80);
        assert_eq!(elide(&"\u{e9}".repeat(80)), "\u{e9}".repeat(80));
    }

    #[gpui_kit::test]
    fn related_rows_navigate_with_the_range_kept(cx: &mut TestAppContext) {
        let snap = snapshot(|w, run| {
            phased_frame(w, run, 0, 1_000, 2);
            request(
                w,
                run,
                "GET",
                "http://h/api/cart",
                Some(200),
                1_005,
                Some(1_015),
            );
            w.push_log(log_at(run, 1_010, LogLevel::Warning));
        });
        let (shell, cx) = open_perf(cx, snap.clone());
        let range = Some(TimeRange::new(
            SessionTime(900 * MS),
            SessionTime(1_100 * MS),
        ));
        let to = location(&shell, cx, 0, range);
        shell.update_in(cx, |s, w, cx| s.navigate(to.clone(), w, cx));
        cx.run_until_parked();
        let req = label(cx, "details-request-0").expect("a request row");
        assert!(req.starts_with("GET /api/cart"), "{req}");
        assert!(req.contains("200") && req.contains("10 ms"), "{req}");
        let log = label(cx, "details-log-0").expect("a log row");
        assert!(
            log.contains("Warning") && log.contains("line at 1010"),
            "{log}"
        );

        click(cx, "details-request-0");
        let id = snap.net.get(0).unwrap().id;
        shell.read_with(cx, |s, _| {
            assert_eq!(s.current().page, Page::Tool(Tool::Network));
            assert_eq!(s.current().selection, Some(Selection::Request(id)));
            assert_eq!(s.current().range, range);
        });
        shell.update_in(cx, |s, w, cx| s.navigate(to.clone(), w, cx));
        cx.run_until_parked();
        click(cx, "details-log-0");
        let id = snap.logs.get_row(0).unwrap().id;
        shell.read_with(cx, |s, _| {
            assert_eq!(s.current().page, Page::Tool(Tool::Logs));
            assert_eq!(s.current().selection, Some(Selection::Log(id)));
            assert_eq!(s.current().range, range);
        });
    }

    #[gpui_kit::test]
    fn new_session_clears_the_pane(cx: &mut TestAppContext) {
        let (shell, cx, fake) = open_shell_fake(cx);
        fake.snaps.send(phase_session()).unwrap();
        fake.status.send(connected("demo", true)).unwrap();
        cx.run_until_parked();
        cx.dispatch_action(ShowPerformance);
        select(&shell, cx, 0);
        assert!(exists(cx, "frame-details"));
        crate::test_support::press(cx, "cmd-n", "ctrl-n");
        fake.snaps.send(snapshot(|_, _| {})).unwrap();
        cx.run_until_parked();
        assert!(!exists(cx, "frame-details"));
    }

    #[gpui_kit::test]
    fn the_analysis_is_not_rerun_for_an_unchanged_frame(cx: &mut TestAppContext) {
        let (shell, cx, fake) = open_shell_fake(cx);
        let (mut w, run) = writer();
        phased_frame(&mut w, run, 0, 100, 2);
        w.push_frame(frame_at(run, 1, 140, 5, 5));
        fake.snaps.send(w.publish()).unwrap();
        fake.status.send(connected("demo", true)).unwrap();
        cx.run_until_parked();
        cx.dispatch_action(ShowPerformance);
        select(&shell, cx, 0);
        let runs = |shell: &Entity<MainWindow>, cx: &mut VisualTestContext| {
            shell.read_with(cx, |s, cx| s.performance_view().read(cx).details_runs())
        };
        let before = runs(&shell, cx);
        assert_eq!(before, 1);
        // A snapshot that adds a frame leaves this frame's phases and coverage alone.
        w.push_frame(frame_at(run, 2, 160, 5, 5));
        fake.snaps.send(w.publish()).unwrap();
        cx.run_until_parked();
        assert_eq!(runs(&shell, cx), before);
        // New CPU coverage changes what the pane shows, so it runs again.
        w.set_cpu_coverage(run, SessionTime(10_000 * MS));
        fake.snaps.send(w.publish()).unwrap();
        cx.run_until_parked();
        assert_eq!(runs(&shell, cx), before + 1);
    }
}
