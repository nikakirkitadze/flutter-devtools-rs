//! The app-wide owner of the capture runtime and app discovery.
//!
//! [`CaptureHost`] is a GPUI global installed a few frames after the first window draws, so
//! startup does not pay for it. Each window then asks it for its own capture.

use fdt_capture::{Capture, CaptureConfig, CaptureRuntime, ConnStatus, Target};
use fdt_store::Snapshot;
use fdt_store::net::{BodyKind, BodyLimits};
use fdt_store::time::EntryId;
use fdt_vm::discovery::{DiscoveredApp, Discovery, DiscoveryConfig};
use gpui_kit::{App, Global};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::watch;

use crate::settings::Settings;

/// What a window can tell its capture to do, and the two streams it reads back.
///
/// A trait so tests can stand in a fake that records commands.
pub trait CaptureControl: 'static {
    fn connect(&self, target: Target);
    fn disconnect(&self);
    fn stop_waiting(&self);
    fn set_recording(&self, on: bool);
    fn new_session(&self);
    fn load_full_body(&self, id: EntryId, kind: BodyKind);
    fn set_free_app_http_memory(&self, on: bool);
    fn set_body_limits(&self, limits: BodyLimits);
    fn fetch_cpu_samples(&self);
    fn status(&self) -> watch::Receiver<ConnStatus>;
    fn snapshots(&self) -> watch::Receiver<Arc<Snapshot>>;
}

impl CaptureControl for Capture {
    fn connect(&self, target: Target) {
        Capture::connect(self, target);
    }
    fn disconnect(&self) {
        Capture::disconnect(self);
    }
    fn stop_waiting(&self) {
        Capture::stop_waiting(self);
    }
    fn set_recording(&self, on: bool) {
        Capture::set_recording(self, on);
    }
    fn new_session(&self) {
        Capture::new_session(self);
    }
    fn load_full_body(&self, id: EntryId, kind: BodyKind) {
        Capture::load_full_body(self, id, kind);
    }
    fn set_free_app_http_memory(&self, on: bool) {
        Capture::set_free_app_http_memory(self, on);
    }
    fn set_body_limits(&self, limits: BodyLimits) {
        Capture::set_body_limits(self, limits);
    }
    fn fetch_cpu_samples(&self) {
        Capture::fetch_cpu_samples(self);
    }
    fn status(&self) -> watch::Receiver<ConnStatus> {
        Capture::status(self)
    }
    fn snapshots(&self) -> watch::Receiver<Arc<Snapshot>> {
        Capture::snapshots(self)
    }
}

/// One capture runtime and one discovery scan for the whole app.
pub struct CaptureHost {
    // Declared first so the scan stops before the runtime goes away.
    _discovery: Discovery,
    runtime: CaptureRuntime,
    apps: watch::Receiver<Arc<[DiscoveredApp]>>,
}

impl Global for CaptureHost {}

/// Why the capture runtime could not start. Set instead of [`CaptureHost`]; the Start page shows
/// it.
pub struct CaptureHostFailure(pub String);

impl Global for CaptureHostFailure {}

impl CaptureHost {
    /// Installs the host with the real discovery settings (the DTD registry, and `dart` from
    /// `PATH` as the fallback). Does nothing if it is already installed. If the runtime cannot
    /// start, windows stay detached and [`CaptureHostFailure`] is set for the Start page.
    pub fn install(cx: &mut App) {
        let config = DiscoveryConfig {
            dart_cli: find_dart(std::env::var_os("PATH").as_deref()),
            ..DiscoveryConfig::default()
        };
        Self::install_with(cx, config);
    }

    /// Like [`install`](Self::install), with an explicit discovery config. Tests pass an empty
    /// temporary registry directory and no `dart`, so they never touch the real tooling daemon.
    pub fn install_with(cx: &mut App, config: DiscoveryConfig) {
        if cx.has_global::<CaptureHost>() {
            return;
        }
        let runtime = match CaptureRuntime::new() {
            Ok(rt) => rt,
            Err(err) => {
                log::error!("could not start the capture runtime: {err}");
                cx.set_global(CaptureHostFailure(err.to_string()));
                return;
            }
        };
        let (discovery, apps) = Discovery::spawn(&runtime.handle(), config);
        cx.set_global(CaptureHost {
            _discovery: discovery,
            runtime,
            apps,
        });
    }

    /// A new capture in `Idle`, sharing the host's discovery for reconnects.
    pub fn new_capture(&self, config: CaptureConfig) -> Box<dyn CaptureControl> {
        Box::new(Capture::start(
            self.runtime.handle(),
            config,
            Some(self.apps.clone()),
        ))
    }

    /// The running apps discovery currently sees.
    pub fn apps(&self) -> watch::Receiver<Arc<[DiscoveredApp]>> {
        self.apps.clone()
    }
}

/// The capture settings the user's preferences describe.
pub fn config_from_settings(s: &Settings) -> CaptureConfig {
    CaptureConfig {
        body_limits: body_limits_from_settings(s),
        free_app_http_memory: s.free_app_http_memory,
        ..CaptureConfig::default()
    }
}

pub fn body_limits_from_settings(s: &Settings) -> BodyLimits {
    BodyLimits {
        per_body: (s.body_cap_kib as usize).saturating_mul(1024),
        total: (s.body_budget_mib as usize).saturating_mul(1 << 20),
    }
}

/// The first `dart` (`dart.bat` on Windows) in the `PATH` value, if any.
pub fn find_dart(path: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let name = if cfg!(windows) { "dart.bat" } else { "dart" };
    std::env::split_paths(path?)
        .map(|dir| dir.join(name))
        .find(|candidate| is_file(candidate))
}

fn is_file(p: &Path) -> bool {
    p.is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_dart_searches_path_in_order() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let name = if cfg!(windows) { "dart.bat" } else { "dart" };
        std::fs::write(second.path().join(name), "").unwrap();
        let joined = std::env::join_paths([first.path(), second.path()]).unwrap();
        assert_eq!(find_dart(Some(&joined)), Some(second.path().join(name)));
        std::fs::write(first.path().join(name), "").unwrap();
        assert_eq!(find_dart(Some(&joined)), Some(first.path().join(name)));
    }

    #[test]
    fn fake_capture_logs_a_cpu_fetch() {
        let (fake, handle) = crate::test_support::fake_capture();
        CaptureControl::fetch_cpu_samples(&fake);
        assert_eq!(handle.commands(), vec!["fetch_cpu".to_string()]);
    }

    #[test]
    fn find_dart_is_none_without_a_match() {
        let dir = tempfile::tempdir().unwrap();
        let joined = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(find_dart(Some(&joined)), None);
        assert_eq!(find_dart(None), None);
    }

    #[test]
    fn config_uses_the_body_settings() {
        let s = Settings {
            body_cap_kib: 64,
            body_budget_mib: 10,
            free_app_http_memory: true,
            ..Settings::default()
        };
        let c = config_from_settings(&s);
        assert_eq!(c.body_limits.per_body, 64 * 1024);
        assert_eq!(c.body_limits.total, 10 << 20);
        assert!(c.free_app_http_memory);
    }
}
