//! Finding Flutter apps opened on iOS simulators and Android devices without `flutter run`, by
//! watching device logs for the VM service line the engine prints at startup.

pub mod android;
pub mod follow;
pub mod marker;
pub mod simulators;
pub mod tools;

use std::time::Duration;

/// How often the watchers check things, so tests can run them fast.
#[derive(Clone, Copy, Debug)]
pub struct WatchTiming {
    /// Interval between liveness checks of found apps.
    pub liveness: Duration,
    /// How long a connection to a forwarded port must stay open to count as alive.
    pub alive_wait: Duration,
    /// Interval between simulator device-file scans.
    pub sim_scan: Duration,
}

impl Default for WatchTiming {
    fn default() -> Self {
        WatchTiming {
            liveness: Duration::from_secs(2),
            alive_wait: Duration::from_millis(400),
            sim_scan: Duration::from_secs(2),
        }
    }
}

/// What one platform's watcher is doing, for the UI.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum PlatformState {
    #[default]
    Off,
    Watching {
        devices: usize,
    },
    Unavailable(String),
}

/// The kind of device a candidate app runs on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Platform {
    IosSimulator,
    Android,
}

impl Platform {
    /// The value the VM reports as `getVM.operatingSystem` for apps on this platform.
    pub fn os(self) -> &'static str {
        match self {
            Platform::IosSimulator => "ios",
            Platform::Android => "android",
        }
    }
}

/// An app found in a device log, not yet confirmed to be a live VM service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub platform: Platform,
    pub device_id: String,
    pub device_name: String,
    pub pid: u32,
    /// An `http://127.0.0.1:<port>/<token>/` URL reachable from this machine.
    pub vm_uri: String,
}
