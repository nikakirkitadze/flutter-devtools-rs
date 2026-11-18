//! Helpers shared by the app's UI tests.

use crate::keymap::platform;
use crate::shell::MainWindow;
use gpui_kit::base::Root;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AppContext as _, Entity, TestAppContext, VisualTestContext};
use std::cell::RefCell;

/// App-level setup shared with production, without fonts or the OS motion probe.
pub fn init(cx: &mut TestAppContext) {
    cx.update(crate::app::init);
}

/// Opens the shell in a test window with focus on it.
pub fn open_shell(cx: &mut TestAppContext) -> (Entity<MainWindow>, &mut VisualTestContext) {
    init(cx);
    let slot: RefCell<Option<Entity<MainWindow>>> = RefCell::new(None);
    let (_, vcx) = cx.add_window_view(|window, cx| {
        let shell = cx.new(|cx| MainWindow::new(window, cx));
        *slot.borrow_mut() = Some(shell.clone());
        Root::new(shell, window, cx)
    });
    let shell = slot.into_inner().expect("shell was built");
    let focus = shell.read_with(vcx, |s, _| s.focus_handle());
    vcx.update(|window, cx| window.focus(&focus, cx));
    vcx.run_until_parked();
    (shell, vcx)
}

/// Whether a test-supported element with `id` is in the next rendered frame.
pub fn exists(cx: &mut VisualTestContext, id: &'static str) -> bool {
    cx.update(|window, cx| {
        window.render_frame(cx);
        window.try_find(id).is_some()
    })
}

/// Presses `mac` on macOS and `other` elsewhere, then lets the app settle.
pub fn press(cx: &mut VisualTestContext, mac: &'static str, other: &'static str) {
    cx.simulate_keystrokes(platform(mac, other));
    cx.run_until_parked();
}
