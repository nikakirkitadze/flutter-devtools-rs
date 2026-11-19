//! The pure part of the Logs list: which rows exist, and whether the list follows new lines.
//! No GPUI.

use std::collections::BTreeMap;
use std::sync::Arc;

/// A row shows at most this many characters of a record's first line.
pub const LOG_ROW_MAX_CHARS: usize = 512;
/// An expanded record shows at most this many lines after the first.
pub const MAX_INLINE_LINES: u32 = 200;

/// The first line of `text`, cut to [`LOG_ROW_MAX_CHARS`] characters, and whether it was cut.
pub fn display_line(text: &str) -> (&str, bool) {
    let line = text.split('\n').next().unwrap_or("");
    match line.char_indices().nth(LOG_ROW_MAX_CHARS) {
        Some((at, _)) => (&line[..at], true),
        None => (line, false),
    }
}

/// What one list row shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowRef {
    /// The record's own row.
    Record { seq: u32 },
    /// Line `line` (1-based after the first) of an expanded record.
    Continuation { seq: u32, line: u32 },
    /// The row after a cut expansion: "Show all lines in details".
    ShowAll { seq: u32 },
}

/// Maps list rows to records, with expanded records taking extra rows.
#[derive(Clone, Debug)]
pub struct RowMap {
    filtered: Arc<[u32]>,
    /// Seq to the number of lines after the first.
    expanded: BTreeMap<u32, u32>,
    index: Vec<Expansion>,
}

#[derive(Clone, Copy, Debug)]
struct Expansion {
    seq: u32,
    /// Position of the record in `filtered`.
    idx: usize,
    /// The row of the record itself.
    start: usize,
    continuations: u32,
    show_all: bool,
}

impl Expansion {
    fn extra(&self) -> usize {
        self.continuations as usize + usize::from(self.show_all)
    }
}

impl RowMap {
    pub fn new(filtered: Arc<[u32]>) -> Self {
        Self {
            filtered,
            expanded: BTreeMap::new(),
            index: Vec::new(),
        }
    }

    /// Replaces the records, keeping the expansion of those still present.
    pub fn set_filtered(&mut self, filtered: Arc<[u32]>) {
        self.filtered = filtered;
        self.rebuild();
    }

    /// Recomputes the index from `expanded`, dropping records no longer listed.
    fn rebuild(&mut self) {
        let filtered = &self.filtered;
        self.expanded
            .retain(|seq, _| filtered.binary_search(seq).is_ok());
        self.index.clear();
        let mut added = 0usize;
        for (&seq, &extra_lines) in &self.expanded {
            let idx = filtered.binary_search(&seq).expect("retained above");
            let e = Expansion {
                seq,
                idx,
                start: idx + added,
                continuations: extra_lines.min(MAX_INLINE_LINES),
                show_all: extra_lines > MAX_INLINE_LINES,
            };
            added += e.extra();
            self.index.push(e);
        }
    }

    pub fn filtered(&self) -> &Arc<[u32]> {
        &self.filtered
    }

    /// Expands `seq`, or collapses it when expanded. `extra_lines` is its line count minus one;
    /// with none there is nothing to expand.
    pub fn toggle(&mut self, seq: u32, extra_lines: u32) {
        if self.expanded.remove(&seq).is_none() {
            if extra_lines == 0 || self.filtered.binary_search(&seq).is_err() {
                return;
            }
            self.expanded.insert(seq, extra_lines);
        }
        self.rebuild();
    }

    pub fn is_expanded(&self, seq: u32) -> bool {
        self.expanded.contains_key(&seq)
    }

    pub fn row_count(&self) -> usize {
        self.filtered.len() + self.index.last().map_or(0, |e| e.start - e.idx + e.extra())
    }

    /// O(log e) in the number of expanded records.
    pub fn resolve(&self, row: usize) -> RowRef {
        let at = self.index.partition_point(|e| e.start <= row);
        let Some(e) = at.checked_sub(1).map(|i| &self.index[i]) else {
            return RowRef::Record {
                seq: self.filtered[row],
            };
        };
        let offset = row - e.start;
        if offset == 0 {
            RowRef::Record { seq: e.seq }
        } else if offset <= e.continuations as usize {
            RowRef::Continuation {
                seq: e.seq,
                line: offset as u32,
            }
        } else if offset == e.continuations as usize + 1 && e.show_all {
            RowRef::ShowAll { seq: e.seq }
        } else {
            // Past this block: every expansion up to and including it added rows.
            let idx = row - (e.start - e.idx + e.extra());
            RowRef::Record {
                seq: self.filtered[idx],
            }
        }
    }

    /// The row of the record `seq`, if it is in the list.
    pub fn row_of(&self, seq: u32) -> Option<usize> {
        let idx = self.filtered.binary_search(&seq).ok()?;
        let before = self.index.partition_point(|e| e.idx < idx);
        let added = before.checked_sub(1).map_or(0, |i| {
            self.index[i].start - self.index[i].idx + self.index[i].extra()
        });
        Some(idx + added)
    }
}

/// Whether the list follows new lines, and how many arrived while it did not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Follow {
    pub following: bool,
    pub unseen: usize,
}

impl Default for Follow {
    fn default() -> Self {
        Self {
            following: true,
            unseen: 0,
        }
    }
}

impl Follow {
    /// `n` rows arrived while the list was (`at_end`) at its end. True when the list should
    /// scroll to the bottom; otherwise the rows count as unseen.
    pub fn on_new_rows(&mut self, n: usize, at_end: Option<bool>) -> bool {
        if self.following && at_end != Some(false) {
            true
        } else {
            self.unseen += n;
            false
        }
    }

    pub fn on_select(&mut self) {
        self.following = false;
    }

    pub fn on_user_scrolled_up(&mut self) {
        self.following = false;
    }

    pub fn resume(&mut self) {
        self.following = true;
        self.unseen = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: u32) -> Arc<[u32]> {
        (0..n).collect::<Vec<_>>().into()
    }

    #[test]
    fn row_map_without_expansion_is_identity() {
        let m = RowMap::new(Arc::from(vec![3u32, 7, 9]));
        assert_eq!(m.row_count(), 3);
        assert_eq!(m.resolve(1), RowRef::Record { seq: 7 });
        assert_eq!(m.row_of(9), Some(2));
        assert_eq!(m.row_of(4), None);
    }

    #[test]
    fn expanding_inserts_continuation_rows() {
        let mut m = RowMap::new(ids(5));
        m.toggle(2, 3);
        assert_eq!(m.row_count(), 8);
        assert_eq!(m.resolve(1), RowRef::Record { seq: 1 });
        assert_eq!(m.resolve(2), RowRef::Record { seq: 2 });
        assert_eq!(m.resolve(3), RowRef::Continuation { seq: 2, line: 1 });
        assert_eq!(m.resolve(5), RowRef::Continuation { seq: 2, line: 3 });
        assert_eq!(m.resolve(6), RowRef::Record { seq: 3 });
        assert_eq!(m.row_of(3), Some(6));
        assert!(m.is_expanded(2));
        m.toggle(2, 3);
        assert_eq!(m.row_count(), 5);
        assert!(!m.is_expanded(2));
        // A single-line record has nothing to expand.
        m.toggle(1, 0);
        assert_eq!(m.row_count(), 5);
    }

    #[test]
    fn expansion_is_capped_with_a_show_all_row() {
        let mut m = RowMap::new(ids(3));
        m.toggle(1, 9_999);
        assert_eq!(m.row_count(), 3 + 200 + 1);
        assert_eq!(
            m.resolve(1 + 200),
            RowRef::Continuation { seq: 1, line: 200 }
        );
        assert_eq!(m.resolve(1 + 201), RowRef::ShowAll { seq: 1 });
        assert_eq!(m.resolve(1 + 202), RowRef::Record { seq: 2 });
        // Exactly at the cap: no ShowAll row.
        let mut m = RowMap::new(ids(3));
        m.toggle(1, 200);
        assert_eq!(m.row_count(), 3 + 200);
    }

    #[test]
    fn resolve_and_row_of_round_trip_with_many_expansions() {
        let n = 5_000u32;
        let filtered: Arc<[u32]> = (0..n).map(|i| i * 2).collect::<Vec<_>>().into();
        let mut m = RowMap::new(filtered.clone());
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..1_000 {
            let seq = (next() % n as u64) as u32 * 2;
            let extra = (next() % 260) as u32;
            m.toggle(seq, extra);
        }
        let mut seen = 0usize;
        for row in 0..m.row_count() {
            match m.resolve(row) {
                RowRef::Record { seq } => {
                    assert_eq!(m.row_of(seq), Some(row));
                    seen += 1;
                }
                RowRef::Continuation { seq, line } => {
                    let start = m.row_of(seq).unwrap();
                    assert_eq!(row, start + line as usize);
                }
                RowRef::ShowAll { seq } => {
                    let start = m.row_of(seq).unwrap();
                    assert_eq!(row, start + 201);
                }
            }
        }
        assert_eq!(seen, n as usize);
    }

    #[test]
    fn set_filtered_keeps_expansion_of_present_records() {
        let mut m = RowMap::new(ids(5));
        m.toggle(1, 2);
        m.toggle(4, 2);
        m.set_filtered(Arc::from(vec![0u32, 1, 2]));
        assert_eq!(m.row_count(), 5);
        assert!(m.is_expanded(1));
        assert!(!m.is_expanded(4));
    }

    #[test]
    fn display_line_cuts_on_a_char_boundary() {
        let long = "é".repeat(100_000);
        let (shown, cut) = display_line(&long);
        assert!(cut);
        assert_eq!(shown.chars().count(), LOG_ROW_MAX_CHARS);
        assert_eq!(display_line("one\ntwo"), ("one", false));
        assert_eq!(display_line("short"), ("short", false));
        let exact = "a".repeat(LOG_ROW_MAX_CHARS);
        assert_eq!(display_line(&exact), (exact.as_str(), false));
        assert_eq!(display_line(""), ("", false));
    }

    #[test]
    fn follow_state_machine() {
        let mut f = Follow::default();
        assert!(f.following);
        assert!(f.on_new_rows(5, None));
        assert!(f.on_new_rows(5, Some(true)));
        assert_eq!(f.unseen, 0);
        // Following, but the list was scrolled away from the end.
        assert!(!f.on_new_rows(3, Some(false)));
        assert_eq!(f.unseen, 3);
        f.resume();
        assert!(f.following && f.unseen == 0);
        f.on_select();
        assert!(!f.following);
        assert!(!f.on_new_rows(4, Some(true)));
        assert_eq!(f.unseen, 4);
        f.resume();
        f.on_user_scrolled_up();
        assert!(!f.following);
        assert!(!f.on_new_rows(1, None));
        assert_eq!(f.unseen, 1);
    }
}
