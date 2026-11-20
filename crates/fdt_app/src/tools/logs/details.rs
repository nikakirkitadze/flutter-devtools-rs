//! The Logs details pane: a record's metadata, its Flutter error sections and its full message
//! in a virtualised list, with Copy and Copy as JSON.

use super::model::{line_at, line_starts, shown_highlights};
use super::view::{LogsView, format_time, level_color};
use crate::nav::{Location, Page, Selection, Tool};
use crate::tools::OpenLocation;
use crate::tools::related::{Kind, RelatedState, RelatedView, status_label};
use fdt_store::logs::LogRow;
use fdt_store::time::EntryId;
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{alpha, font, layout, radius, row, space};
use gpui_kit::{
    AnyElement, ClipboardItem, Context, HighlightStyle, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, StyledText,
    TestSupportExt as _, UniformListScrollHandle, div, relative, uniform_list,
};
use serde::Serialize;
use std::ops::Range;
use std::sync::Arc;

/// A record across sessions: run ids restart at 1 in every session, so an id alone is not unique.
pub(super) type RecordKey = (u64, EntryId);

/// The details pane's per-record caches and scroll positions.
pub(super) struct DetailsState {
    /// The record the line index is for, and where each of its lines starts.
    lines: Option<(RecordKey, Arc<Vec<u32>>)>,
    message_scroll: UniformListScrollHandle,
    stack_scroll: UniformListScrollHandle,
    /// The frame and requests around the selected record, computed off the UI thread.
    pub(super) related: RelatedState,
}

impl Default for DetailsState {
    fn default() -> Self {
        Self {
            lines: None,
            message_scroll: UniformListScrollHandle::new(),
            stack_scroll: UniformListScrollHandle::new(),
            related: RelatedState::default(),
        }
    }
}

impl DetailsState {
    pub(super) fn reset(&mut self) {
        self.lines = None;
        self.related.clear();
    }

    #[cfg(test)]
    pub(super) fn line_count(&self) -> usize {
        self.lines.as_ref().map_or(0, |(_, l)| l.len())
    }
}

/// The record as the "Copy as JSON" button writes it.
#[derive(Serialize)]
struct JsonRecord<'a> {
    time: String,
    level: &'a str,
    source: &'a str,
    logger: &'a str,
    run: u32,
    message: &'a str,
}

/// `{"time":"0:12.345","level":"Error","source":"Flutter error","logger":"","run":1,"message":"…"}`
pub(super) fn record_json(rec: &LogRow<'_>) -> String {
    serde_json::to_string(&JsonRecord {
        time: format_time(rec.time),
        level: rec.level.label(),
        source: rec.source.label(),
        logger: rec.logger,
        run: rec.id.run.0,
        message: rec.text,
    })
    .expect("a record of plain strings serializes")
}

impl LogsView {
    /// Starts (or skips) the correlation for the selected record while the pane is open.
    pub(super) fn sync_related(&mut self, cx: &mut Context<Self>) {
        let target = self
            .selected
            .filter(|_| self.details_open)
            .and_then(|seq| Some((self.snap.logs.get_row(seq as usize)?.id, seq as usize)));
        let Some(target) = target else {
            self.details.related.clear();
            return;
        };
        let snap = self.snap.clone();
        self.details.related.sync(
            cx,
            |v| &mut v.details.related,
            |v, cx| v.sync_related(cx),
            snap,
            target,
            Kind::Log,
        );
    }

    /// Asks the shell to open `selection` in `tool`, keeping the range, with a Back step.
    pub(super) fn open_related(
        &mut self,
        tool: Tool,
        selection: Selection,
        cx: &mut Context<Self>,
    ) {
        cx.emit(OpenLocation(Location {
            page: Page::Tool(tool),
            selection: Some(selection),
            range: self.filter.range,
        }));
    }

    fn render_related(
        &self,
        view: &RelatedView,
        t: &fdt_ui::tokens::Palette,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let link = |id: SharedString, text: String| {
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
        };
        let muted = |id: &'static str, text: &'static str| {
            div()
                .id(id)
                .test_support()
                .aria_label(text)
                .text_size(font::LABEL)
                .font_weight(font::SEMIBOLD)
                .text_color(Hsla::from(t.text_muted))
                .child(text)
        };
        let mut block = div()
            .id("logs-related")
            .test_support()
            .flex()
            .flex_col()
            .flex_none()
            .gap(space::S2)
            .child(muted("logs-related-heading", "Related"));
        match view.frame {
            Some(f) => {
                let id = f.id;
                block = block.child(
                    link(
                        "logs-related-frame".into(),
                        format!("Frame #{} \u{b7} {}", f.number, status_label(f.status)),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.open_related(Tool::Performance, Selection::Frame(id), cx)
                    })),
                );
            }
            None => {
                block = block.child(
                    div()
                        .id("logs-related-empty")
                        .test_support()
                        .aria_label("No frame at this moment.")
                        .text_color(Hsla::from(t.text_muted))
                        .child("No frame at this moment."),
                );
            }
        }
        if !view.requests.is_empty() {
            block = block.child(muted("logs-related-requests-heading", "Requests in flight"));
            for (i, r) in view.requests.iter().enumerate() {
                let id = r.id;
                block = block.child(
                    link(format!("logs-related-request-{i}").into(), r.text.clone()).on_click(
                        cx.listener(move |this, _, _, cx| {
                            this.open_related(Tool::Network, Selection::Request(id), cx)
                        }),
                    ),
                );
            }
        }
        block.into_any_element()
    }

    pub(super) fn copy_text(&self, seq: u32, cx: &mut Context<Self>) {
        let Some(rec) = self.snap.logs.get_row(seq as usize) else {
            return;
        };
        let text = rec.text.to_string();
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    pub(super) fn copy_json(&self, seq: u32, cx: &mut Context<Self>) {
        let Some(rec) = self.snap.logs.get_row(seq as usize) else {
            return;
        };
        let json = record_json(&rec);
        cx.write_to_clipboard(ClipboardItem::new_string(json));
    }

    /// The text of `text` with the current search's matches highlighted.
    pub(super) fn highlighted(
        &self,
        text: String,
        ranges: Vec<Range<usize>>,
        bg: Hsla,
    ) -> AnyElement {
        if ranges.is_empty() {
            return text.into_any_element();
        }
        StyledText::new(text)
            .with_highlights(ranges.into_iter().map(|r| {
                (
                    r,
                    HighlightStyle {
                        background_color: Some(bg),
                        ..HighlightStyle::default()
                    },
                )
            }))
            .into_any_element()
    }

    pub(super) fn render_details(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let t = *cx.tokens();
        let shell = div()
            .id("logs-details")
            .test_support()
            .size_full()
            .flex()
            .flex_col()
            .gap(space::S8)
            .p(space::S12)
            .border_l(layout::BORDER)
            .border_color(Hsla::from(t.border))
            .bg(Hsla::from(t.surface))
            .text_size(font::BODY)
            .text_color(Hsla::from(t.text));
        let rec = self
            .selected
            .and_then(|seq| self.snap.logs.get_row(seq as usize));
        let (Some(seq), Some(rec)) = (self.selected, rec) else {
            return shell
                .child(
                    div()
                        .text_color(Hsla::from(t.text_muted))
                        .child("Select a log line to see its details."),
                )
                .into_any_element();
        };
        if self.details.lines.as_ref().map(|l| l.0) != Some((self.snap.session, rec.id)) {
            self.details.lines =
                Some(((self.snap.session, rec.id), Arc::new(line_starts(rec.text))));
            self.details.message_scroll = UniformListScrollHandle::new();
            self.details.stack_scroll = UniformListScrollHandle::new();
        }
        let line_count = self.details.lines.as_ref().map_or(0, |(_, l)| l.len());
        let run = format!("Run {}", rec.id.run.0);
        let logger = if rec.logger.is_empty() {
            "\u{2014}".to_string()
        } else {
            rec.logger.to_string()
        };
        let level_colour = level_color(rec.level, &t);
        let level = rec.level.label();
        let source = rec.source.label();
        let time = format_time(rec.time);
        let clamped = rec.clamped;
        let error = rec.error.cloned();
        let related = self
            .details
            .related
            .view(self.snap.session, rec.id)
            .map(|v| self.render_related(&v, &t, cx));

        let meta = |id: &'static str, key: &'static str, value: String, colour: Hsla| {
            div()
                .flex()
                .gap(space::S8)
                .child(
                    div()
                        .flex_none()
                        .w(table_key_width())
                        .text_color(Hsla::from(t.text_muted))
                        .child(key),
                )
                .child(
                    div()
                        .id(id)
                        .test_support()
                        .aria_label(SharedString::from(value.clone()))
                        .min_w_0()
                        .text_color(colour)
                        .child(value),
                )
        };
        let button = |id: &'static str, label: &'static str| {
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
        };
        let heading = |id: &'static str, title: &'static str| {
            div()
                .id(id)
                .test_support()
                .aria_label(title)
                .text_size(font::LABEL)
                .font_weight(font::SEMIBOLD)
                .text_color(Hsla::from(t.text_muted))
                .child(title)
        };

        let mut body = shell
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(space::S8)
                    .child(div().flex_1().font_weight(font::SEMIBOLD).child("Details"))
                    .child(
                        button("logs-copy-text", "Copy")
                            .on_click(cx.listener(move |this, _, _, cx| this.copy_text(seq, cx))),
                    )
                    .child(
                        button("logs-copy-json", "Copy as JSON")
                            .on_click(cx.listener(move |this, _, _, cx| this.copy_json(seq, cx))),
                    )
                    .child(button("logs-details-close", "Close").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.close_details(cx);
                        },
                    ))),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(space::S2)
                    .child(meta("details-time", "Time", time, Hsla::from(t.text)))
                    .child(meta(
                        "details-level",
                        "Level",
                        level.to_string(),
                        level_colour,
                    ))
                    .child(meta(
                        "details-source",
                        "Source",
                        source.to_string(),
                        Hsla::from(t.text),
                    ))
                    .child(meta("details-logger", "Logger", logger, Hsla::from(t.text)))
                    .child(meta("details-run", "Run", run, Hsla::from(t.text)))
                    .children(clamped.then(|| {
                        div()
                            .id("details-clamped")
                            .test_support()
                            .aria_label("Before fdt connected")
                            .text_size(font::LABEL)
                            .text_color(Hsla::from(t.text_muted))
                            .child("Before fdt connected")
                    })),
            )
            .children(related);

        if let Some(err) = error {
            body = body
                .child(heading("details-section-summary", "Summary"))
                .child(
                    div()
                        .id("details-summary")
                        .test_support()
                        .aria_label(SharedString::from(err.summary.clone()))
                        .child(err.summary.clone()),
                );
            if err.widget.is_some() || err.location.is_some() {
                body = body.child(heading("details-section-widget", "Caused by widget"));
                if let Some(widget) = err.widget.clone() {
                    body = body.child(
                        div()
                            .id("details-widget")
                            .test_support()
                            .aria_label(SharedString::from(widget.clone()))
                            .font_family(font::MONO)
                            .text_size(font::DENSE)
                            .child(widget),
                    );
                }
                if let Some(location) = err.location.clone() {
                    body = body.child(
                        div()
                            .id("details-location")
                            .test_support()
                            .aria_label(SharedString::from(location.clone()))
                            .font_family(font::MONO)
                            .text_size(font::DENSE)
                            .text_color(Hsla::from(t.text_muted))
                            .child(location),
                    );
                }
            }
            if !err.stack.is_empty() {
                let frames = err.stack.len();
                body = body
                    .child(heading("details-section-stack", "Stack trace"))
                    .child(
                        div().h(row::LOG * frames.min(10) as f32).flex_none().child(
                            uniform_list(
                                "logs-details-stack",
                                frames,
                                cx.processor(|this, range: Range<usize>, _w, cx| {
                                    this.render_stack_rows(range, cx)
                                }),
                            )
                            .track_scroll(&self.details.stack_scroll)
                            .size_full(),
                        ),
                    );
            }
        }
        body.child(heading("details-section-message", "Message"))
            .child(
                div().flex_1().min_h_0().child(
                    uniform_list(
                        "logs-details-lines",
                        line_count,
                        cx.processor(|this, range: Range<usize>, _w, cx| {
                            this.render_detail_lines(range, cx)
                        }),
                    )
                    .track_scroll(&self.details.message_scroll)
                    .size_full(),
                ),
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

    /// Lines `range` of the selected record's message. Only the lines on screen are built.
    fn render_detail_lines(
        &mut self,
        range: Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let t = *cx.tokens();
        let Some(seq) = self.selected else {
            return Vec::new();
        };
        let Some(rec) = self.snap.logs.get_row(seq as usize) else {
            return Vec::new();
        };
        // The index belongs to one record; for any other it would cut inside the wrong text.
        let Some((id, starts)) = self.details.lines.clone() else {
            return Vec::new();
        };
        if id != (self.snap.session, rec.id) {
            return Vec::new();
        }
        let bg = t.accent.with_alpha(alpha::SELECTION);
        range
            .map(|i| {
                let (line, cut) = line_at(rec.text, &starts, i);
                let ranges = self
                    .matcher
                    .as_ref()
                    .map(|m| m.ranges(line))
                    .unwrap_or_default();
                let mut shown = line.to_string();
                if cut {
                    shown.push('\u{2026}');
                }
                let label = SharedString::from(line.to_string());
                Self::mono_row()
                    .id(SharedString::from(format!("details-line-{i}")))
                    .test_support()
                    .aria_label(label)
                    .truncate()
                    .child(self.highlighted(shown, ranges, bg))
                    .into_any_element()
            })
            .collect()
    }

    fn render_stack_rows(
        &mut self,
        range: Range<usize>,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let t = *cx.tokens();
        let Some(seq) = self.selected else {
            return Vec::new();
        };
        let Some(err) = self.snap.logs.get_row(seq as usize).and_then(|r| r.error) else {
            return Vec::new();
        };
        range
            .filter_map(|i| {
                let frame = err.stack.get(i)?;
                let (shown, _) = super::model::display_line(frame);
                Some(
                    Self::mono_row()
                        .id(SharedString::from(format!("details-stack-{i}")))
                        .test_support()
                        .aria_label(SharedString::from(frame.clone()))
                        .truncate()
                        .text_color(Hsla::from(t.text_muted))
                        .child(shown.to_string())
                        .into_any_element(),
                )
            })
            .collect()
    }

    /// Match ranges of the current text filter inside `text` (empty without one).
    pub(super) fn matcher_ranges(&self, text: &str) -> Vec<Range<usize>> {
        self.matcher
            .as_ref()
            .map(|m| shown_highlights(text, m))
            .unwrap_or_default()
    }
}

fn table_key_width() -> gpui_kit::Pixels {
    fdt_ui::tokens::table::COL_LEVEL + space::S8
}

#[cfg(test)]
mod tests {
    use super::*;
    use fdt_store::logs::{LogInput, LogLevel, LogSource};
    use fdt_store::time::SessionTime;

    #[test]
    fn json_has_the_documented_shape_and_escapes() {
        let (mut w, run) = crate::test_support::writer();
        w.push_log(LogInput {
            run,
            time: SessionTime(12_345_000_000),
            clamped: false,
            level: LogLevel::Error,
            source: LogSource::FlutterError,
            logger: String::new(),
            text: "bad \"thing\"\nline two".to_string(),
            error: None,
        });
        let snap = w.publish();
        let json = record_json(&snap.logs.row(0));
        assert_eq!(
            json,
            r#"{"time":"0:12.345","level":"Error","source":"Flutter error","logger":"","run":1,"message":"bad \"thing\"\nline two"}"#
        );
    }
}
