//! Most-recently-used command ids.

/// How many recent command ids are kept.
pub const RECENT_CAPACITY: usize = 20;

/// Moves `id` to the front (inserting it if new) and keeps at most
/// [`RECENT_CAPACITY`] entries.
pub fn record_recent(recent: &mut Vec<String>, id: &str) {
    recent.retain(|r| r != id);
    recent.insert(0, id.to_string());
    recent.truncate(RECENT_CAPACITY);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_recent_moves_to_front_and_caps() {
        let mut r = Vec::new();
        for id in ["a", "b", "a"] {
            record_recent(&mut r, id);
        }
        assert_eq!(r, vec!["a", "b"]);

        let mut r = Vec::new();
        for i in 0..25 {
            record_recent(&mut r, &format!("id{i}"));
        }
        assert_eq!(r.len(), RECENT_CAPACITY);
        assert_eq!(r[0], "id24");
        assert_eq!(r[RECENT_CAPACITY - 1], "id5");
    }
}
