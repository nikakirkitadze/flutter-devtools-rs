//! Locating the external tools device discovery uses (`adb`, `xcrun`), without running anything.

use std::path::{Path, PathBuf};

use crate::discovery::Os;

/// Finds `adb`: each `PATH` entry, then `$ANDROID_HOME` and `$ANDROID_SDK_ROOT`
/// (`platform-tools`), then on macOS Homebrew's `/opt/homebrew/bin` and `/usr/local/bin` (an
/// app started from Finder has neither on `PATH`), then the default SDK location for the OS.
/// Empty variables are ignored.
pub fn find_adb(
    os: Os,
    env: &dyn Fn(&str) -> Option<String>,
    home: Option<PathBuf>,
    exists: &dyn Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let set = |k: &str| env(k).filter(|v| !v.is_empty());
    let name = if os == Os::Windows { "adb.exe" } else { "adb" };
    let sep = if os == Os::Windows { ';' } else { ':' };

    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(path) = set("PATH") {
        dirs.extend(path.split(sep).filter(|p| !p.is_empty()).map(PathBuf::from));
    }
    for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(d) = set(var) {
            dirs.push(PathBuf::from(d).join("platform-tools"));
        }
    }
    match os {
        Os::MacOs => {
            dirs.push(PathBuf::from("/opt/homebrew/bin"));
            dirs.push(PathBuf::from("/usr/local/bin"));
            if let Some(h) = &home {
                dirs.push(h.join("Library/Android/sdk/platform-tools"));
            }
        }
        Os::Windows => {
            if let Some(l) = set("LOCALAPPDATA") {
                dirs.push(
                    PathBuf::from(l)
                        .join("Android")
                        .join("Sdk")
                        .join("platform-tools"),
                );
            } else if let Some(h) = &home {
                dirs.push(h.join("AppData/Local/Android/Sdk/platform-tools"));
            }
        }
        Os::Linux => {
            if let Some(h) = &home {
                dirs.push(h.join("Android/Sdk/platform-tools"));
            }
        }
    }
    dirs.into_iter().map(|d| d.join(name)).find(|p| exists(p))
}

/// `/usr/bin/xcrun` on macOS; `None` elsewhere or when it is missing.
pub fn find_xcrun(exists: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    if Os::current() != Os::MacOs {
        return None;
    }
    let p = PathBuf::from("/usr/bin/xcrun");
    exists(&p).then_some(p)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    #[test]
    fn adb_is_found_in_order() {
        let home = Some(PathBuf::from("/home/u"));
        assert_eq!(
            find_adb(
                Os::Linux,
                &env_of(&[("PATH", "/a:/b")]),
                home.clone(),
                &|_| true
            ),
            Some(PathBuf::from("/a/adb"))
        );
        let want = PathBuf::from("/sdk/platform-tools/adb");
        assert_eq!(
            find_adb(
                Os::Linux,
                &env_of(&[("ANDROID_HOME", "/missing"), ("ANDROID_SDK_ROOT", "/sdk")]),
                home.clone(),
                &|p| p == want
            ),
            Some(want.clone())
        );
        let mac = PathBuf::from("/home/u/Library/Android/sdk/platform-tools/adb");
        assert_eq!(
            find_adb(Os::MacOs, &env_of(&[]), home.clone(), &|p| p == mac),
            Some(mac.clone())
        );
        let win = PathBuf::from("/a").join("adb.exe");
        assert_eq!(
            find_adb(Os::Windows, &env_of(&[("PATH", "/a")]), None, &|p| p == win),
            Some(win)
        );
        // Empty variables are ignored.
        assert_eq!(
            find_adb(Os::Linux, &env_of(&[("ANDROID_HOME", "")]), None, &|_| true),
            None
        );
    }

    #[test]
    fn homebrew_adb_is_found_on_macos_after_the_env_and_before_the_sdk() {
        let home = Some(PathBuf::from("/Users/u"));
        // Started from Finder: PATH has no Homebrew folder.
        let finder = env_of(&[("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")]);
        let sdk = PathBuf::from("/Users/u/Library/Android/sdk/platform-tools/adb");
        for brew in ["/opt/homebrew/bin/adb", "/usr/local/bin/adb"] {
            let brew = PathBuf::from(brew);
            assert_eq!(
                find_adb(Os::MacOs, &finder, home.clone(), &|p| p == brew),
                Some(brew.clone())
            );
            assert_eq!(
                find_adb(Os::MacOs, &finder, home.clone(), &|p| p == brew || p == sdk),
                Some(brew.clone()),
                "before the default SDK"
            );
            let env_sdk = PathBuf::from("/sdk/platform-tools/adb");
            assert_eq!(
                find_adb(
                    Os::MacOs,
                    &env_of(&[("ANDROID_HOME", "/sdk")]),
                    home.clone(),
                    &|p| p == brew || p == env_sdk
                ),
                Some(env_sdk),
                "after $ANDROID_HOME"
            );
        }
        let apple = PathBuf::from("/opt/homebrew/bin/adb");
        let intel = PathBuf::from("/usr/local/bin/adb");
        assert_eq!(
            find_adb(Os::MacOs, &finder, home.clone(), &|p| p == apple
                || p == intel),
            Some(apple)
        );
        // Only on macOS.
        assert_eq!(find_adb(Os::Linux, &finder, home, &|p| p == intel), None);
    }

    #[test]
    fn missing_adb_is_none() {
        assert_eq!(
            find_adb(
                Os::MacOs,
                &env_of(&[("PATH", "/a")]),
                Some(PathBuf::from("/h")),
                &|_| false
            ),
            None
        );
    }
}
