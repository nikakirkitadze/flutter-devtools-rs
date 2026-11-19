//! Whether the release-mode budget tests assert their absolute limits.
//!
//! The budgets in the spec are for a reference machine. Hosted CI runners are slower and noisier,
//! so a budget test always runs and prints what it measured, but only fails the build when
//! `FDT_BUDGETS=strict` is set (the default on a reference machine run).

/// The environment variable that turns budget measurements into assertions.
pub const ENV: &str = "FDT_BUDGETS";

/// True when `FDT_BUDGETS=strict`.
pub fn strict() -> bool {
    std::env::var(ENV).is_ok_and(|v| v == "strict")
}

/// Asserts `ok` when budgets are strict; otherwise only reports a miss.
#[track_caller]
pub fn check(ok: bool, what: impl FnOnce() -> String) {
    if ok {
        return;
    }
    if strict() {
        panic!("budget missed: {}", what());
    }
    eprintln!(
        "budget missed (report only; set {ENV}=strict to fail): {}",
        what()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_passing_check_never_panics() {
        check(true, || unreachable!());
    }
}
