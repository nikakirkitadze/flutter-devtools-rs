//! Maps device clocks onto the host-anchored session time axis.

use crate::time::SessionTime;
use std::time::{SystemTime, UNIX_EPOCH};

/// Offsets measured once per run against the device clocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Calibration {
    pub skew_us: i64,
    pub timeline_to_wall_us: i64,
}

/// A mapped time; `clamped` means the event happened before the origin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub time: SessionTime,
    pub clamped: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunClock {
    pub origin_host_us: i64,
    pub skew_us: i64,
    pub timeline_to_wall_us: i64,
}

impl RunClock {
    pub fn new(origin_host_us: i64, cal: Calibration) -> Self {
        Self {
            origin_host_us,
            skew_us: cal.skew_us,
            timeline_to_wall_us: cal.timeline_to_wall_us,
        }
    }

    pub fn from_epoch_ms(&self, ms: i64) -> Stamp {
        self.from_epoch_us(ms.saturating_mul(1000))
    }

    pub fn from_epoch_us(&self, wall_us: i64) -> Stamp {
        let rel = wall_us
            .saturating_add(self.skew_us)
            .saturating_sub(self.origin_host_us);
        stamp(rel)
    }

    pub fn from_timeline_us(&self, timeline_us: i64) -> Stamp {
        self.from_epoch_us(timeline_us.saturating_add(self.timeline_to_wall_us))
    }
}

fn stamp(rel_us: i64) -> Stamp {
    if rel_us < 0 {
        Stamp {
            time: SessionTime(0),
            clamped: true,
        }
    } else {
        Stamp {
            time: SessionTime((rel_us as u64).saturating_mul(1000)),
            clamped: false,
        }
    }
}

pub fn calibrate(
    host_before_us: i64,
    host_after_us: i64,
    device_wall_us: i64,
    timeline_us: i64,
) -> Calibration {
    Calibration {
        skew_us: (host_before_us + host_after_us) / 2 - device_wall_us,
        timeline_to_wall_us: device_wall_us - timeline_us,
    }
}

pub fn host_now_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calibrate_maps_device_clocks_onto_the_host_axis() {
        let cal = calibrate(1_000_000, 1_000_200, 2_000_000, 500);
        let clock = RunClock::new(1_000_000, cal);
        assert_eq!(clock.from_epoch_us(2_000_000).time, SessionTime(100_000));
        assert_eq!(clock.from_timeline_us(500).time, SessionTime(100_000));
        assert_eq!(clock.from_epoch_ms(2_000).time, SessionTime(100_000));
        let early = clock.from_epoch_us(1_000_000);
        assert_eq!(early.time, SessionTime(0));
        assert!(early.clamped);
        assert!(!clock.from_epoch_us(2_000_000).clamped);
    }

    #[test]
    fn host_now_is_after_2020() {
        assert!(host_now_us() > 1_577_836_800_000_000);
    }
}
