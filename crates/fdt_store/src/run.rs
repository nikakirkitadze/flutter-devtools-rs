//! Runs: one connection of one app to the session.

use crate::clock::RunClock;
use crate::time::{RunId, SessionTime};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Debug,
    Profile,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppInfo {
    pub name: String,
    pub device: Option<String>,
    pub mode: Mode,
    pub flutter_version: Option<String>,
    pub workspace: Option<String>,
    pub vm_uri: String,
    pub pid: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunInfo {
    pub id: RunId,
    pub app: AppInfo,
    pub started: SessionTime,
    pub ended: Option<SessionTime>,
    pub clock: RunClock,
    /// The display's refresh rate, once known.
    pub refresh_hz: Option<f32>,
    /// Widget-build tracing is on in the app (the run's actual state, not the wish).
    pub tracing: bool,
    /// When tracing was on, oldest first: from the first time until the second, which is `None`
    /// while it is still on. Each time is when the app confirmed the change.
    pub tracing_periods: Vec<(SessionTime, Option<SessionTime>)>,
}

impl RunInfo {
    /// Whether widget-build tracing was on at `t`. A frame is judged by its own start, not by
    /// whether tracing is on now.
    pub fn tracing_at(&self, t: SessionTime) -> bool {
        self.tracing_periods
            .iter()
            .any(|&(on, off)| on <= t && off.is_none_or(|off| t < off))
    }
}
