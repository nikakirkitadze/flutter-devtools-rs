//! Booted iOS simulators, read from the `device.plist` files CoreSimulator keeps on disk, so
//! no process has to run to learn which simulators are up.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::SystemTime;

use regex::Regex;

use crate::discovery::Os;

/// `~/Library/Developer/CoreSimulator/Devices` on macOS.
pub fn simulator_devices_dir(home: Option<PathBuf>) -> Option<PathBuf> {
    if Os::current() != Os::MacOs {
        return None;
    }
    Some(home?.join("Library/Developer/CoreSimulator/Devices"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Simulator {
    pub udid: String,
    pub name: String,
    pub booted: bool,
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Reads `UDID`, `name` and `state` from an XML `device.plist` (state 3 is booted).
pub fn parse_device_plist(xml: &str) -> Option<Simulator> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"<key>([^<]*)</key>\s*<(?:string|integer)>([^<]*)</(?:string|integer)>")
            .expect("valid regex")
    });
    let (mut udid, mut name, mut state) = (None, None, None);
    for c in re.captures_iter(xml) {
        let v = unescape(&c[2]);
        match &c[1] {
            "UDID" => udid = Some(v),
            "name" => name = Some(v),
            "state" => state = v.trim().parse::<i64>().ok(),
            _ => {}
        }
    }
    Some(Simulator {
        udid: udid.filter(|u| !u.is_empty())?,
        name: name?,
        booted: state? == 3,
    })
}

/// Watches the simulator device folders, re-reading only `device.plist` files whose
/// modification time or size changed since the previous call.
pub struct SimulatorScan {
    dir: PathBuf,
    seen: HashMap<PathBuf, (Option<SystemTime>, u64, Option<Simulator>)>,
}

impl SimulatorScan {
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            seen: HashMap::new(),
        }
    }

    /// Booted simulators, sorted by name then udid.
    pub fn booted(&mut self) -> Vec<Simulator> {
        let mut current: HashMap<PathBuf, (Option<SystemTime>, u64, Option<Simulator>)> =
            HashMap::new();
        if let Ok(rd) = std::fs::read_dir(&self.dir) {
            for entry in rd.flatten() {
                let plist: PathBuf = entry.path().join("device.plist");
                let Ok(meta) = std::fs::metadata(&plist) else {
                    continue;
                };
                let key = (meta.modified().ok(), meta.len());
                let sim = match self.seen.remove(&plist) {
                    Some((m, l, sim)) if (m, l) == key => sim,
                    _ => read_plist(&plist),
                };
                current.insert(plist, (key.0, key.1, sim));
            }
        }
        self.seen = current;
        let mut out: Vec<Simulator> = self
            .seen
            .values()
            .filter_map(|(_, _, s)| s.clone())
            .filter(|s| s.booted)
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.udid.cmp(&b.udid)));
        out
    }
}

fn read_plist(path: &Path) -> Option<Simulator> {
    parse_device_plist(&std::fs::read_to_string(path).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plist(udid: &str, name: &str, state: i32) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>UDID</key>
	<string>{udid}</string>
	<key>deviceType</key>
	<string>com.apple.CoreSimulator.SimDeviceType.iPhone-18-Pro</string>
	<key>isDeleted</key>
	<false/>
	<key>lastUsedAt</key>
	<date>2026-11-22T17:21:57Z</date>
	<key>name</key>
	<string>{name}</string>
	<key>runtime</key>
	<string>com.apple.CoreSimulator.SimRuntime.iOS-27-0</string>
	<key>state</key>
	<integer>{state}</integer>
</dict>
</plist>
"#
        )
    }

    #[test]
    fn device_plist_is_parsed() {
        let id = "0B66B701-5F1A-4816-8EA9-AA4499CEFF1B";
        assert_eq!(
            parse_device_plist(&plist(id, "iPhone 18 Pro", 3)),
            Some(Simulator {
                udid: id.into(),
                name: "iPhone 18 Pro".into(),
                booted: true
            })
        );
        assert!(
            !parse_device_plist(&plist(id, "iPhone 18 Pro", 1))
                .unwrap()
                .booted
        );
        assert_eq!(parse_device_plist("garbage"), None);
    }

    fn write(dir: &Path, id: &str, name: &str, state: i32) {
        let d = dir.join(id);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("device.plist"), plist(id, name, state)).unwrap();
    }

    #[test]
    fn scan_rereads_only_changed_files() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "AAAA", "iPhone A", 3);
        write(tmp.path(), "BBBB", "iPhone B", 1);
        let mut scan = SimulatorScan::new(tmp.path().to_path_buf());
        let first = scan.booted();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].udid, "AAAA");

        // A longer name changes the size even within the same mtime second.
        write(tmp.path(), "BBBB", "iPhone B Longer Name", 3);
        let two = scan.booted();
        assert_eq!(two.len(), 2);
        assert_eq!(two[0].udid, "AAAA");

        std::fs::remove_dir_all(tmp.path().join("AAAA")).unwrap();
        let one = scan.booted();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].udid, "BBBB");
    }
}
