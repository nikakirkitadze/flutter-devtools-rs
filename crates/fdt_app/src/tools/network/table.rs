//! The requests table: a `TableDelegate` over the filtered request numbers of a snapshot.
//!
//! It holds only what a visible row needs. Every cell is derived from the shared `Arc`ed
//! record when drawn, so nothing here is proportional to the number of requests except the
//! filtered number list itself.

use super::model::{
    Tone, format_bytes, format_duration_ns, request_json, socket_state, status_text, status_tone,
    waterfall,
};
use crate::nav::{SessionTime, TimeRange};
use crate::shell::strip::format_offset;
use fdt_store::Snapshot;
use fdt_store::net::{HttpRecord, StatusClass};
use fdt_store::net_query::NetTab;
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::{font, layout, radius, space, table};
use gpui_kit::component::Sizable as _;
use gpui_kit::component::Size;
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::table::{Column, TableDelegate, TableState};
use gpui_kit::{
    App, ClipboardItem, Context, Hsla, IntoElement, ParentElement as _, SharedString, Styled as _,
    Window, div, relative,
};
use std::sync::Arc;

#[cfg(test)]
thread_local! {
    /// Cells built by `render_td`, for the "only visible rows" test.
    pub(crate) static CELLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Linear socket lookups made by the WebSockets rows.
    pub(crate) static SOCKET_LOOKUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Rows whose socket cells are kept between draws.
const WS_ROWS_KEPT: usize = 256;

pub(super) struct NetTable {
    pub snap: Arc<Snapshot>,
    /// Sequence numbers of the requests shown, oldest first.
    pub rows: Arc<[u32]>,
    pub tab: NetTab,
    pub span: TimeRange,
    pub now: SessionTime,
    /// The WebSockets cells of the row drawn last, so the lookup is made once per row and not
    /// once per cell.
    ws_rows: std::cell::RefCell<std::collections::HashMap<usize, [String; 4]>>,
}

impl NetTable {
    pub fn new(snap: Arc<Snapshot>) -> Self {
        Self {
            snap,
            rows: Arc::from(Vec::new()),
            tab: NetTab::Requests,
            span: TimeRange::new(SessionTime(0), SessionTime(1)),
            now: SessionTime(0),
            ws_rows: std::cell::RefCell::new(std::collections::HashMap::new()),
        }
    }

    /// Shows `rows` of `snap` under `tab`.
    pub fn replace(&mut self, snap: Arc<Snapshot>, rows: Arc<[u32]>, tab: NetTab) {
        self.snap = snap;
        self.rows = rows;
        self.tab = tab;
        self.ws_rows.get_mut().clear();
    }

    /// A WebSockets cell (1 to 4) of `row`, looking the socket up once per row.
    fn ws_cell(&self, row: usize, r: &HttpRecord, col: usize) -> String {
        let mut cached = self.ws_rows.borrow_mut();
        // A frame draws a few dozen rows; a long scroll must not keep them all.
        if cached.len() > WS_ROWS_KEPT {
            cached.clear();
        }
        let cells = cached.entry(row).or_insert_with(|| self.ws_cells(r));
        cells[col.min(4) - 1].clone()
    }

    /// State, sent, received and last activity of a WebSockets row. One linear socket lookup.
    fn ws_cells(&self, r: &HttpRecord) -> [String; 4] {
        #[cfg(test)]
        SOCKET_LOOKUPS.with(|c| c.set(c.get() + 1));
        let socket = self.snap.net.socket_for(r.id);
        [
            socket_state(socket).to_string(),
            socket.map_or_else(dash, |s| format_bytes(s.write_bytes)),
            socket.map_or_else(dash, |s| format_bytes(s.read_bytes)),
            socket
                .and_then(|s| s.last_read.max(s.last_write))
                .map_or_else(dash, format_offset),
        ]
    }

    fn record(&self, row: usize) -> Option<&Arc<HttpRecord>> {
        self.snap.net.get(*self.rows.get(row)? as usize)
    }

    /// The text of a cell, `None` for the waterfall.
    fn text(&self, row: usize, col: usize) -> Option<String> {
        let r = self.record(row)?;
        if self.tab == NetTab::WebSockets {
            return Some(match col {
                0 => r.url.clone(),
                _ => self.ws_cell(row, r, col),
            });
        }
        match col {
            0 => Some(r.method.clone()),
            1 => Some(r.host.clone()),
            2 => Some(r.path.clone()),
            3 => Some(status_text(r)),
            4 => Some(r.duration_ns().map_or_else(dash, format_duration_ns)),
            5 => Some(r.size().map_or_else(dash, format_bytes)),
            _ => None,
        }
    }
}

fn dash() -> String {
    "\u{2013}".to_string()
}

impl TableDelegate for NetTable {
    fn columns_count(&self, _: &App) -> usize {
        if self.tab == NetTab::WebSockets { 5 } else { 7 }
    }

    fn rows_count(&self, _: &App) -> usize {
        self.rows.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> Column {
        if self.tab == NetTab::WebSockets {
            return match col_ix {
                0 => Column::new("url", "URL").width(table::COL_URL),
                1 => Column::new("state", "State").width(table::COL_STATE),
                2 => Column::new("sent", "Sent")
                    .width(table::COL_BYTES)
                    .text_right(),
                3 => Column::new("received", "Received")
                    .width(table::COL_BYTES)
                    .text_right(),
                _ => Column::new("activity", "Last activity").width(table::COL_ACTIVITY),
            };
        }
        match col_ix {
            0 => Column::new("method", "Method").width(table::COL_METHOD),
            1 => Column::new("host", "Host").width(table::COL_HOST),
            2 => Column::new("path", "Path").width(table::COL_PATH),
            3 => Column::new("status", "Status").width(table::COL_STATUS),
            4 => Column::new("duration", "Duration")
                .width(table::COL_DURATION)
                .text_right(),
            5 => Column::new("size", "Size")
                .width(table::COL_SIZE)
                .text_right(),
            _ => Column::new("waterfall", "Waterfall").width(layout::WATERFALL_WIDTH),
        }
    }

    fn cell_label(&self, row_ix: usize, col_ix: usize, _: &App) -> Option<SharedString> {
        self.text(row_ix, col_ix).map(Into::into)
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        #[cfg(test)]
        CELLS.with(|c| c.set(c.get() + 1));
        let t = *cx.tokens();
        let cell = div().w_full().truncate().text_size(font::DENSE);
        let Some(r) = self.record(row_ix).cloned() else {
            return cell.into_any_element();
        };
        if self.tab == NetTab::WebSockets {
            let text = match col_ix {
                0 => r.url.clone(),
                _ => self.ws_cell(row_ix, &r, col_ix),
            };
            return match col_ix {
                1 | 4 => cell.text_color(Hsla::from(t.text_muted)),
                _ => cell,
            }
            .child(text)
            .into_any_element();
        }
        let tone = |tone: Tone| match tone {
            Tone::Success => Hsla::from(t.success),
            Tone::Muted => Hsla::from(t.text_muted),
            Tone::Warning => Hsla::from(t.warning),
            Tone::Danger => Hsla::from(t.danger),
        };
        match col_ix {
            3 => {
                let colour = tone(status_tone(&r));
                let mut el = div()
                    .flex()
                    .items_center()
                    .gap(space::S4)
                    .text_size(font::DENSE)
                    .text_color(colour);
                if r.status_class() == StatusClass::Pending {
                    el = el.child(Spinner::new().with_size(Size::XSmall).color(colour));
                }
                el.child(status_text(&r)).into_any_element()
            }
            4 | 5 => cell
                .text_color(Hsla::from(t.text_muted))
                .font_features(font::tabular_figures())
                .child(self.text(row_ix, col_ix).unwrap_or_default())
                .into_any_element(),
            6 => {
                let (x, w) = waterfall(&r, self.span, self.now);
                div()
                    .relative()
                    .w_full()
                    .h(space::S8)
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .h_full()
                            .left(relative(x))
                            .w(relative(w))
                            .rounded(radius::CHIP)
                            .bg(tone(status_tone(&r))),
                    )
                    .into_any_element()
            }
            1 => cell
                .text_color(Hsla::from(t.text_muted))
                .child(r.host.clone())
                .into_any_element(),
            _ => cell
                .child(self.text(row_ix, col_ix).unwrap_or_default())
                .into_any_element(),
        }
    }

    fn context_menu(
        &mut self,
        row_ix: usize,
        menu: PopupMenu,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> PopupMenu {
        let Some(r) = self.record(row_ix) else {
            return menu;
        };
        let url = r.url.clone();
        let json = request_json(r);
        menu.item(PopupMenuItem::new("Copy URL").on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(url.clone()));
        }))
        .item(
            PopupMenuItem::new("Copy as JSON").on_click(move |_, _, cx| {
                cx.write_to_clipboard(ClipboardItem::new_string(json.clone()));
            }),
        )
    }
}
