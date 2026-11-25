//! The Memory tool: the memory chart, the allocation profile, leak findings and heap snapshots.

pub mod chart;
pub mod classes_tab;
pub mod details;
pub mod leaks_tab;
pub mod snapshot_tab;
pub mod view;

pub use leaks_tab::LeaksTab;
pub use snapshot_tab::SnapshotTab;
pub use view::{MemoryTab, MemoryView, TAKE_SNAPSHOT_TIP};

use fdt_store::heap::HeapId;

/// The number a heap snapshot goes by in the UI: the first one taken is #1.
pub fn heap_number(id: HeapId) -> u64 {
    u64::from(id.0) + 1
}

/// `1204331` as `1,204,331`.
pub fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_group_by_thousands() {
        for (n, want) in [
            (0, "0"),
            (999, "999"),
            (1_000, "1,000"),
            (1_204_331, "1,204,331"),
            (12_345, "12,345"),
        ] {
            assert_eq!(group_digits(n), want);
        }
    }

    #[test]
    fn the_first_snapshot_is_number_one() {
        assert_eq!(heap_number(HeapId(0)), 1);
        assert_eq!(heap_number(HeapId(6)), 7);
    }
}
