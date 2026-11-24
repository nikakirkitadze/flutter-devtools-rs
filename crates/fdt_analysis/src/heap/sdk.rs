//! The one versioned table of SDK-internal names the leak scan depends on (§14 call 10).
//!
//! A snapshot has no class hierarchy, so the scan finds `State`, `Element` and `ChangeNotifier`
//! by library URI and recognises their subclasses by field-name prefix. Everything that can
//! drift between Flutter releases lives here: when a release renames a class or moves a
//! library, add a row, and nothing else changes.

/// Classes whose references the heap does not retain: `(class name, library uri)`.
pub const WEAK: &[(&str, &str)] = &[
    ("_WeakProperty", "dart:core"),
    ("_WeakReferenceImpl", "dart:core"),
    ("FinalizerEntry", "dart:_internal"),
];

/// A Flutter minor version, `(major, minor)`.
pub type Minor = (u32, u32);

#[derive(Debug, PartialEq, Eq)]
pub struct SdkFields {
    pub framework_uri: &'static str,
    pub notifier_uri: &'static str,
    /// `(class, the field set to null on unmount, the debug-only lifecycle field)`.
    pub state: (&'static str, &'static str, &'static str),
    /// `(class, lifecycle field, widget field, lifecycle enum class, enum name field, the
    /// defunct name)`.
    pub element: (
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
    ),
    /// `(class, listeners field, the debug-only disposed flag)`.
    pub notifier: (&'static str, &'static str, &'static str),
    pub weak: &'static [(&'static str, &'static str)],
    /// `(class, library uri, the static `package:http_profile` clients fill)`.
    pub http: (&'static str, &'static str, &'static str),
    pub mint: (&'static str, &'static str),
}

const fn row(notifier_uri: &'static str) -> SdkFields {
    SdkFields {
        framework_uri: "package:flutter/src/widgets/framework.dart",
        notifier_uri,
        state: ("State", "_element", "_debugLifecycleState"),
        element: (
            "Element",
            "_lifecycleState",
            "_widget",
            "_ElementLifecycle",
            "_name",
            "defunct",
        ),
        notifier: ("ChangeNotifier", "_listeners", "_debugDisposed"),
        weak: WEAK,
        http: ("_HttpProfileData", "dart:_http", "_developerProfilingData"),
        mint: ("_Mint", "dart:core"),
    }
}

/// Rows by the first Flutter minor they apply to, oldest first. Flutter 3.41 and 3.44 share
/// the 3.41 row; in 3.50 `ChangeNotifier` moved to `package:listen` (fact H17).
pub const TABLE: &[(Minor, SdkFields)] = &[
    (
        (3, 41),
        row("package:flutter/src/foundation/change_notifier.dart"),
    ),
    ((3, 50), row("package:listen/src/listen.dart")),
];

/// `"3.47.9"` or `"3.47.9-0.1.pre"` to `(3, 47)`.
fn parse_minor(version: &str) -> Option<Minor> {
    let mut parts = version.trim().split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// The row for `flutter_version`, then the others, newest first. A missing or unparsable
/// version gives every row, newest first. The scan uses the first row whose classes exist.
pub fn rows_for(flutter_version: Option<&str>) -> Vec<&'static SdkFields> {
    let newest_first: Vec<&'static (Minor, SdkFields)> = TABLE.iter().rev().collect();
    let Some(minor) = flutter_version.and_then(parse_minor) else {
        return newest_first.into_iter().map(|r| &r.1).collect();
    };
    // The newest row that starts at or before the version; an older version than every row
    // falls to the oldest.
    let own = newest_first
        .iter()
        .position(|r| r.0 <= minor)
        .unwrap_or(newest_first.len() - 1);
    let mut out = vec![&newest_first[own].1];
    out.extend(
        newest_first
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != own)
            .map(|(_, r)| &r.1),
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uris(v: Option<&str>) -> Vec<&'static str> {
        rows_for(v).iter().map(|r| r.notifier_uri).collect()
    }

    #[test]
    fn the_version_picks_its_row_first() {
        assert!(uris(Some("3.47.9"))[0].contains("foundation"));
        assert!(uris(Some("3.50.0"))[0].contains("listen"));
        assert!(uris(Some("3.52.1-0.0.pre"))[0].contains("listen"));
        assert!(uris(Some("3.41.9"))[0].contains("foundation"));
        // Older than every row: the oldest, then the rest.
        assert!(uris(Some("3.10.0"))[0].contains("foundation"));
        assert_eq!(uris(Some("3.47.9")).len(), TABLE.len());
    }

    #[test]
    fn missing_or_odd_versions_give_every_row_newest_first() {
        for v in [None, Some(""), Some("main"), Some("3"), Some("x.y.z")] {
            let u = uris(v);
            assert_eq!(u.len(), TABLE.len(), "{v:?}");
            assert!(u[0].contains("listen"), "{v:?}");
        }
    }
}
