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
}
