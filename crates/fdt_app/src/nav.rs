//! Navigation model: tools, pages, locations and a capped back/forward history.
//! Pure data; no GPUI.

use std::collections::VecDeque;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tool {
    Timeline,
    Performance,
    Memory,
    Network,
    Logs,
    Widgets,
    State,
}

impl Tool {
    pub const ALL: [Tool; 7] = [
        Tool::Timeline,
        Tool::Performance,
        Tool::Memory,
        Tool::Network,
        Tool::Logs,
        Tool::Widgets,
        Tool::State,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Tool::Timeline => "Timeline",
            Tool::Performance => "Performance",
            Tool::Memory => "Memory",
            Tool::Network => "Network",
            Tool::Logs => "Logs",
            Tool::Widgets => "Widgets",
            Tool::State => "State",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Tool::Timeline => "timeline",
            Tool::Performance => "performance",
            Tool::Memory => "memory",
            Tool::Network => "network",
            Tool::Logs => "logs",
            Tool::Widgets => "widgets",
            Tool::State => "state",
        }
    }

    pub fn index(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Page {
    /// Connect to a running app.
    Start,
    Tool(Tool),
    Settings,
}

pub use fdt_store::time::{EntryId, SessionTime, TimeRange};

/// The selected record. Ids carry their run, so a selection survives a reconnect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Selection {
    Log(EntryId),
    Request(EntryId),
    Socket(EntryId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub page: Page,
    pub selection: Option<Selection>,
    pub range: Option<TimeRange>,
}

impl Location {
    pub fn at(page: Page) -> Self {
        Self {
            page,
            selection: None,
            range: None,
        }
    }

    /// Moves to another page, keeping the range and dropping the selection.
    pub fn with_page(&self, page: Page) -> Self {
        Self {
            page,
            selection: None,
            range: self.range,
        }
    }

    /// Drops the selection if any, else the range if any, else `None`.
    pub fn cleared(&self) -> Option<Self> {
        if self.selection.is_some() {
            Some(Self {
                selection: None,
                ..self.clone()
            })
        } else if self.range.is_some() {
            Some(Self {
                range: None,
                ..self.clone()
            })
        } else {
            None
        }
    }
}

pub struct NavHistory {
    back: VecDeque<Location>,
    current: Location,
    forward: Vec<Location>,
    capacity: usize,
}

impl NavHistory {
    pub const DEFAULT_CAPACITY: usize = 100;

    pub fn new(initial: Location) -> Self {
        Self::with_capacity(initial, Self::DEFAULT_CAPACITY)
    }

    pub fn with_capacity(initial: Location, capacity: usize) -> Self {
        Self {
            back: VecDeque::new(),
            current: initial,
            forward: Vec::new(),
            capacity,
        }
    }

    pub fn current(&self) -> &Location {
        &self.current
    }

    pub fn navigate(&mut self, to: Location) -> bool {
        if to == self.current {
            return false;
        }
        let prev = std::mem::replace(&mut self.current, to);
        self.back.push_back(prev);
        if self.back.len() > self.capacity {
            self.back.pop_front();
        }
        self.forward.clear();
        true
    }

    pub fn go_back(&mut self) -> Option<&Location> {
        let prev = self.back.pop_back()?;
        let cur = std::mem::replace(&mut self.current, prev);
        self.forward.push(cur);
        Some(&self.current)
    }

    pub fn go_forward(&mut self) -> Option<&Location> {
        let next = self.forward.pop()?;
        let cur = std::mem::replace(&mut self.current, next);
        self.back.push_back(cur);
        Some(&self.current)
    }

    pub fn can_go_back(&self) -> bool {
        !self.back.is_empty()
    }

    pub fn can_go_forward(&self) -> bool {
        !self.forward.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eid(seq: u64) -> EntryId {
        EntryId {
            run: fdt_store::time::RunId(1),
            seq,
        }
    }

    #[test]
    fn selection_round_trips_through_history() {
        let mut h = NavHistory::new(Location::at(Page::Start));
        let mut l = tl(Tool::Logs);
        l.selection = Some(Selection::Socket(eid(7)));
        h.navigate(l.clone());
        h.navigate(tl(Tool::Network));
        assert_eq!(h.go_back(), Some(&l));
        assert_eq!(h.current().selection, Some(Selection::Socket(eid(7))));
        h.go_back();
        assert_eq!(h.current(), &Location::at(Page::Start));
    }

    fn tl(t: Tool) -> Location {
        Location::at(Page::Tool(t))
    }

    #[test]
    fn tool_metadata() {
        let labels = [
            "Timeline",
            "Performance",
            "Memory",
            "Network",
            "Logs",
            "Widgets",
            "State",
        ];
        let slugs = [
            "timeline",
            "performance",
            "memory",
            "network",
            "logs",
            "widgets",
            "state",
        ];
        assert_eq!(Tool::ALL.len(), 7);
        for (i, t) in Tool::ALL.iter().enumerate() {
            assert_eq!(t.index(), i);
            assert_eq!(t.label(), labels[i]);
            assert_eq!(t.slug(), slugs[i]);
        }
        assert_eq!(Tool::ALL[0], Tool::Timeline);
        assert_eq!(Tool::ALL[6], Tool::State);
    }

    #[test]
    fn navigate_pushes_and_clears_forward() {
        let mut h = NavHistory::new(tl(Tool::Timeline));
        assert!(h.navigate(tl(Tool::Memory)));
        assert_eq!(h.go_back(), Some(&tl(Tool::Timeline)));
        assert!(h.can_go_forward());
        assert!(h.navigate(tl(Tool::Logs)));
        assert!(!h.can_go_forward());
    }

    #[test]
    fn navigating_to_the_current_location_is_ignored() {
        let mut h = NavHistory::new(tl(Tool::Timeline));
        assert!(!h.navigate(tl(Tool::Timeline)));
        assert!(!h.can_go_back());
    }

    #[test]
    fn back_and_forward_round_trip() {
        let mut h = NavHistory::new(tl(Tool::Timeline));
        h.navigate(tl(Tool::Memory));
        h.navigate(Location::at(Page::Settings));
        assert_eq!(h.go_back(), Some(&tl(Tool::Memory)));
        assert_eq!(h.go_back(), Some(&tl(Tool::Timeline)));
        assert_eq!(h.go_back(), None);
        assert_eq!(h.go_forward(), Some(&tl(Tool::Memory)));
        assert_eq!(h.go_forward(), Some(&Location::at(Page::Settings)));
        assert_eq!(h.go_forward(), None);
        assert_eq!(h.current(), &Location::at(Page::Settings));
    }

    #[test]
    fn history_is_capped() {
        let mut h = NavHistory::with_capacity(tl(Tool::Timeline), 100);
        for i in 0..150u64 {
            let mut l = tl(Tool::Timeline);
            l.range = Some(TimeRange::new(SessionTime(i), SessionTime(i + 1)));
            assert!(h.navigate(l));
        }
        let mut n = 0;
        while h.go_back().is_some() {
            n += 1;
        }
        assert_eq!(n, 100);
    }

    #[test]
    fn with_page_keeps_range_and_drops_selection() {
        let mut l = tl(Tool::Timeline);
        l.selection = Some(Selection::Log(eid(1)));
        l.range = Some(TimeRange::new(SessionTime(1), SessionTime(2)));
        let m = l.with_page(Page::Tool(Tool::Memory));
        assert_eq!(m.page, Page::Tool(Tool::Memory));
        assert_eq!(m.range, l.range);
        assert_eq!(m.selection, None);
    }

    #[test]
    fn cleared_drops_selection_then_range() {
        let mut l = tl(Tool::Timeline);
        l.selection = Some(Selection::Request(eid(2)));
        l.range = Some(TimeRange::new(SessionTime(1), SessionTime(2)));
        let a = l.cleared().unwrap();
        assert_eq!(a.selection, None);
        assert_eq!(a.range, l.range);
        let b = a.cleared().unwrap();
        assert_eq!(b, tl(Tool::Timeline));
        assert_eq!(b.cleared(), None);
    }

    #[test]
    fn time_range_orders_its_ends() {
        let r = TimeRange::new(SessionTime(9), SessionTime(3));
        assert_eq!(r.start, SessionTime(3));
        assert_eq!(r.end, SessionTime(9));
    }
}
