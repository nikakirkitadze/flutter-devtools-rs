//! The session time axis and record ids.

/// Nanoseconds since the session origin; continuous across runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SessionTime(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TimeRange {
    pub start: SessionTime,
    pub end: SessionTime,
}

impl TimeRange {
    pub fn new(a: SessionTime, b: SessionTime) -> Self {
        Self {
            start: a.min(b),
            end: a.max(b),
        }
    }

    /// Inclusive at both ends.
    pub fn contains(&self, t: SessionTime) -> bool {
        self.start <= t && t <= self.end
    }

    /// True when `[start, end]` shares at least one instant with this range.
    pub fn overlaps(&self, start: SessionTime, end: SessionTime) -> bool {
        start <= self.end && end >= self.start
    }

    pub fn duration_ns(&self) -> u64 {
        self.end.0 - self.start.0
    }
}

/// One connection of the app to the session. Numbered from 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RunId(pub u32);

/// Session-unique id of a stored record; `seq` is unique per stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EntryId {
    pub run: RunId,
    pub seq: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_range_new_orders_endpoints() {
        let r = TimeRange::new(SessionTime(9), SessionTime(3));
        assert_eq!((r.start, r.end), (SessionTime(3), SessionTime(9)));
        assert_eq!(r.duration_ns(), 6);
    }

    #[test]
    fn contains_and_overlaps_are_inclusive() {
        let r = TimeRange::new(SessionTime(10), SessionTime(20));
        assert!(r.contains(SessionTime(10)) && r.contains(SessionTime(20)));
        assert!(!r.contains(SessionTime(9)) && !r.contains(SessionTime(21)));
        assert!(r.overlaps(SessionTime(0), SessionTime(10)));
        assert!(r.overlaps(SessionTime(20), SessionTime(30)));
        assert!(r.overlaps(SessionTime(12), SessionTime(13)));
        assert!(!r.overlaps(SessionTime(0), SessionTime(9)));
        assert!(!r.overlaps(SessionTime(21), SessionTime(30)));
    }
}
