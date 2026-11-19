//! The shell's static chrome as cached views: the title bar's content, the sidebar and the
//! session strip.
//!
//! Each is a [`Facet`] of the `MainWindow`, so a frame the window was notified for (a scroll
//! reaching up through its ancestors) replays them instead of rebuilding them. The keys list
//! everything each part reads; a part rebuilds only when its key changes.

use crate::facet::Facet;
use crate::shell::MainWindow;
use fdt_ui::theme::TokensExt as _;
use fdt_ui::tokens::layout;
use gpui_kit::{AnyElement, Context, StyleRefinement, Window};
use gpui_kit::{App, AppContext as _, Entity, Styled as _};

pub(super) type Part = Facet<MainWindow>;

pub(super) struct Parts {
    pub title: Entity<Part>,
    pub sidebar: Entity<Part>,
    pub strip: Entity<Part>,
}

impl Parts {
    pub fn new(shell: &Entity<MainWindow>, cx: &mut Context<MainWindow>) -> Self {
        Self {
            title: cx.new(|cx| Facet::new(shell, build_title, title_key, cx)),
            sidebar: cx.new(|cx| Facet::new(shell, build_sidebar, sidebar_key, cx)),
            strip: cx.new(|cx| Facet::new(shell, build_strip, strip_key, cx)),
        }
    }
}

fn build_title(
    shell: &mut MainWindow,
    window: &mut Window,
    cx: &mut Context<MainWindow>,
) -> AnyElement {
    shell.render_title_content(window, cx)
}

fn build_sidebar(
    shell: &mut MainWindow,
    _: &mut Window,
    cx: &mut Context<MainWindow>,
) -> AnyElement {
    shell.render_sidebar(cx)
}

fn build_strip(
    shell: &mut MainWindow,
    window: &mut Window,
    cx: &mut Context<MainWindow>,
) -> AnyElement {
    shell.render_strip(window, cx)
}

fn title_key(shell: &MainWindow, cx: &App) -> String {
    let session = shell.session.read(cx);
    format!(
        "{:?}",
        (
            cx.appearance(),
            shell.history.can_go_back(),
            shell.history.can_go_forward(),
            session.status(),
            session.recording_elapsed().map(|e| e.as_secs()),
            shell.current().range,
        )
    )
}

fn sidebar_key(shell: &MainWindow, cx: &App) -> String {
    format!(
        "{:?}",
        (cx.appearance(), shell.current().page, shell.counts)
    )
}

fn strip_key(shell: &MainWindow, cx: &App) -> String {
    let session = shell.session.read(cx);
    format!(
        "{:?}",
        (
            cx.appearance(),
            session.snapshot().generation,
            session.recording_elapsed(),
            shell.current().range,
        )
    )
}

/// The title bar's content fills the bar beside its window controls.
pub(super) fn title_style() -> StyleRefinement {
    StyleRefinement::default().flex_1().h_full()
}

pub(super) fn sidebar_style() -> StyleRefinement {
    StyleRefinement::default()
        .w(layout::SIDEBAR_WIDTH)
        .h_full()
        .flex_none()
}

pub(super) fn strip_style() -> StyleRefinement {
    StyleRefinement::default().w_full().h(layout::STRIP_HEIGHT)
}
