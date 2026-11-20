//! Which library URIs count as the app's own code.

/// False for native code (no library), the Dart SDK and the Flutter framework and pub cache.
pub fn is_user_library(library: Option<&str>) -> bool {
    const PREFIXES: [&str; 3] = ["dart:", "org-dartlang-sdk:", "package:flutter/"];
    const PATHS: [&str; 5] = [
        "/packages/flutter/",
        "/flutter/packages/",
        "/.pub-cache/",
        "/pub-cache/hosted/",
        "/flutter/bin/cache/",
    ];
    let Some(lib) = library else {
        return false;
    };
    !PREFIXES.iter().any(|p| lib.starts_with(p)) && !PATHS.iter().any(|p| lib.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_code_cases() {
        assert!(is_user_library(Some(
            "file:///Users/xxxx/src/jank_lab/lib/scenarios/jank.dart"
        )));
        assert!(!is_user_library(Some(
            "file:///Users/xxxx/flutter/packages/flutter/lib/src/widgets/framework.dart"
        )));
        assert!(!is_user_library(Some(
            "org-dartlang-sdk:///sdk/lib/core/stopwatch.dart"
        )));
        assert!(!is_user_library(None));
        assert!(!is_user_library(Some("dart:core")));
        assert!(!is_user_library(Some(
            "package:flutter/src/widgets/framework.dart"
        )));
        assert!(!is_user_library(Some(
            "file:///Users/x/.pub-cache/hosted/pub.dev/foo-1.0.0/lib/foo.dart"
        )));
        assert!(!is_user_library(Some(
            "file:///c/flutter/bin/cache/pkg/sky_engine/lib/ui/ui.dart"
        )));
        assert!(is_user_library(Some("package:jank_lab/main.dart")));
    }
}
