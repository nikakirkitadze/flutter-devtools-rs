//! A request for focus that is granted only to an element that is drawn, and never from a dialog.
//!
//! Pages ask for focus when they open, but the element they want may not exist yet (an empty
//! page) and the user may be typing in the palette. Focus on an element that is not drawn sends
//! keys nowhere, so a request is granted while drawing, and only if it can be honoured then.

use gpui_kit::component::WindowExt as _;
use gpui_kit::{App, FocusHandle, Window};

/// Whether a page's canvas should take focus the next time it is drawn.
#[derive(Debug, Default)]
pub struct PendingFocus {
    pending: bool,
}

impl PendingFocus {
    /// Asks for the focus.
    pub fn request(&mut self) {
        self.pending = true;
    }

    /// Withdraws the request.
    pub fn cancel(&mut self) {
        self.pending = false;
    }

    /// Grants the request, during render, and returns whether `target` took the focus.
    ///
    /// See [`PendingFocus::due`] for when that is.
    pub fn take(
        &mut self,
        window: &mut Window,
        cx: &mut App,
        target: &FocusHandle,
        drawn: bool,
    ) -> bool {
        let due = self.due(window, cx, drawn);
        if due {
            window.focus(target, cx);
        }
        due
    }

    /// Spends the request if it can be granted now, for an element that cannot hand out a
    /// [`FocusHandle`] and takes the focus itself.
    ///
    /// Due only when a request is pending, `drawn` is true and no dialog is open. A dialog keeps
    /// the request pending for when it closes. A render with `drawn == false` (an empty page)
    /// clears it, so data that arrives later never takes the focus from wherever it is.
    pub fn due(&mut self, window: &mut Window, cx: &mut App, drawn: bool) -> bool {
        if !self.pending {
            return false;
        }
        if !drawn {
            self.pending = false;
            return false;
        }
        if window.has_active_dialog(cx) {
            return false;
        }
        self.pending = false;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::open_shell;
    use gpui_kit::TestAppContext;

    fn handle(cx: &mut gpui_kit::VisualTestContext) -> FocusHandle {
        cx.update(|_, cx| cx.focus_handle())
    }

    #[gpui_kit::test]
    fn pending_focus_cases(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell(cx);
        let target = handle(cx);
        let mut pending = PendingFocus::default();

        // Nothing requested: nothing happens.
        let took = cx.update(|w, cx| pending.take(w, cx, &target, true));
        assert!(!took);

        // No dialog and drawn: it focuses, once.
        pending.request();
        let took = cx.update(|w, cx| pending.take(w, cx, &target, true));
        assert!(took);
        assert!(cx.update(|w, _| target.is_focused(w)));
        let again = cx.update(|w, cx| pending.take(w, cx, &target, true));
        assert!(!again, "a granted request is spent");

        // Not drawn: the request is cleared, so later data does not take the focus.
        let other = handle(cx);
        cx.update(|w, cx| w.focus(&other, cx));
        pending.request();
        assert!(!cx.update(|w, cx| pending.take(w, cx, &target, false)));
        assert!(!cx.update(|w, cx| pending.take(w, cx, &target, true)));
        assert!(cx.update(|w, _| other.is_focused(w)));

        // A dialog keeps it pending, and it is granted once the dialog closes.
        let shell_focus = shell.read_with(cx, |s, _| s.focus_handle());
        cx.update(|w, cx| w.focus(&shell_focus, cx));
        crate::test_support::press(cx, "cmd-k", "ctrl-k");
        pending.request();
        assert!(!cx.update(|w, cx| pending.take(w, cx, &target, true)));
        assert!(pending.pending);
        cx.update(|w, cx| w.close_dialog(cx));
        cx.run_until_parked();
        assert!(cx.update(|w, cx| pending.take(w, cx, &target, true)));
    }
}
