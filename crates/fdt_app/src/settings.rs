//! Persisted user settings: the theme preference and recent palette commands.
//!
//! The pure parts (`Settings`, `SettingsStore`) need no GPUI. The file is read
//! once before the event loop starts; writes go through the background executor.

use fdt_ui::theme::ThemePreference;
use gpui_kit::{App, Global};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

/// How many recently used app addresses are kept.
pub const RECENT_URLS_MAX: usize = 5;

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct Settings {
    pub theme: ThemePreference,
    pub recent_commands: Vec<String>,
    /// Stop an app's HTTP profiling when fdt disconnects, to free its memory.
    pub free_app_http_memory: bool,
    /// Largest response body fetched, in KiB.
    pub body_cap_kib: u32,
    /// Total memory for kept response bodies, in MiB.
    pub body_budget_mib: u32,
    /// Trace widget builds in an app from the moment of connecting. Off: it adds about 90 % to
    /// build time.
    pub trace_widget_builds: bool,
    /// App addresses used recently, newest first.
    pub recent_urls: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemePreference::default(),
            recent_commands: Vec::new(),
            free_app_http_memory: false,
            body_cap_kib: 1024,
            body_budget_mib: 200,
            trace_widget_builds: false,
            recent_urls: Vec::new(),
        }
    }
}

/// Writes `bytes` to `path`, readable by the owner only on unix: the file holds recent VM
/// service URLs, which carry auth tokens.
fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write as _;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        // `mode` only applies to a new file; a leftover temp file may be looser.
        use std::os::unix::fs::PermissionsExt as _;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)
}

impl Settings {
    /// Reads every known field on its own: a field of the wrong type keeps its default and logs
    /// one warning. Unknown fields are ignored.
    pub fn from_map(map: &serde_json::Map<String, serde_json::Value>) -> Self {
        fn field<T: serde::de::DeserializeOwned>(
            map: &serde_json::Map<String, serde_json::Value>,
            name: &str,
            out: &mut T,
        ) {
            let Some(value) = map.get(name) else { return };
            match serde_json::from_value(value.clone()) {
                Ok(v) => *out = v,
                Err(err) => log::warn!("ignoring settings field `{name}`: {err}"),
            }
        }
        let mut s = Self::default();
        field(map, "theme", &mut s.theme);
        field(map, "recent_commands", &mut s.recent_commands);
        field(map, "free_app_http_memory", &mut s.free_app_http_memory);
        field(map, "body_cap_kib", &mut s.body_cap_kib);
        field(map, "body_budget_mib", &mut s.body_budget_mib);
        field(map, "trace_widget_builds", &mut s.trace_widget_builds);
        field(map, "recent_urls", &mut s.recent_urls);
        s.recent_urls.truncate(RECENT_URLS_MAX);
        s
    }
}

/// Puts `url` first, dropping any earlier copy, and keeps at most `RECENT_URLS_MAX`.
pub fn record_recent_url(urls: &mut Vec<String>, url: &str) {
    urls.retain(|u| u != url);
    urls.insert(0, url.to_string());
    urls.truncate(RECENT_URLS_MAX);
}

/// Holds the current settings and where they are saved.
pub struct SettingsStore {
    path: Option<PathBuf>,
    current: Settings,
}

impl SettingsStore {
    /// `<config dir>/fdt/settings.json`, when the platform has a config directory.
    pub fn default_path() -> Option<PathBuf> {
        dirs::config_dir().map(|dir| dir.join("fdt").join("settings.json"))
    }

    /// Reads `path`. A missing, unreadable or invalid file gives the defaults.
    pub fn load(path: PathBuf) -> Self {
        let current = match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
                Ok(serde_json::Value::Object(map)) => Settings::from_map(&map),
                Ok(_) => {
                    log::warn!("ignoring settings file {}: not an object", path.display());
                    Settings::default()
                }
                Err(err) => {
                    log::warn!("ignoring invalid settings file {}: {err}", path.display());
                    Settings::default()
                }
            },
            Err(err) if err.kind() == io::ErrorKind::NotFound => Settings::default(),
            Err(err) => {
                log::warn!("could not read settings file {}: {err}", path.display());
                Settings::default()
            }
        };
        Self {
            path: Some(path),
            current,
        }
    }

    /// A store with no file: saves are no-ops.
    pub fn in_memory() -> Self {
        Self {
            path: None,
            current: Settings::default(),
        }
    }

    pub fn get(&self) -> &Settings {
        &self.current
    }

    /// Writes `settings` to `path` through a temporary file, so a crash never leaves half a file.
    pub fn write(path: &Path, settings: &Settings) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_vec_pretty(settings).map_err(io::Error::other)?;
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        write_private(&tmp, &json)?;
        std::fs::rename(&tmp, path)
    }
}

struct AppSettings {
    store: SettingsStore,
    /// Bumped on every update; a save only writes while it is still the latest.
    generation: Arc<AtomicU64>,
    /// The generation last written to disk; equal to `generation` when nothing is pending.
    saved: Arc<AtomicU64>,
    /// Serialises writes so they never share the temporary file.
    write_lock: Arc<Mutex<()>>,
}

impl Global for AppSettings {}

pub fn init_settings(cx: &mut App, store: SettingsStore) {
    cx.set_global(AppSettings {
        store,
        generation: Arc::new(AtomicU64::new(0)),
        saved: Arc::new(AtomicU64::new(0)),
        write_lock: Arc::new(Mutex::new(())),
    });
}

pub fn settings(cx: &App) -> &Settings {
    cx.global::<AppSettings>().store.get()
}

/// Changes the settings now and saves them in the background. A failed save is logged.
pub fn update_settings(cx: &mut App, f: impl FnOnce(&mut Settings)) {
    let app = cx.global_mut::<AppSettings>();
    f(&mut app.store.current);
    let generation = app.generation.fetch_add(1, Ordering::SeqCst) + 1;
    let Some(path) = app.store.path.clone() else {
        return;
    };
    let snapshot = app.store.current.clone();
    let latest = app.generation.clone();
    let saved = app.saved.clone();
    let lock = app.write_lock.clone();
    cx.background_executor()
        .spawn(async move {
            let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            // A newer update has queued its own write; skip this stale one.
            if latest.load(Ordering::SeqCst) != generation {
                return;
            }
            match SettingsStore::write(&path, &snapshot) {
                Ok(()) => saved.store(generation, Ordering::SeqCst),
                Err(err) => log::warn!("saving settings to {} failed: {err}", path.display()),
            }
        })
        .detach();
}

/// Writes the current settings to disk now, on the calling thread, if they have changed since
/// the last save.
///
/// Used when the app quits: macOS then calls `exit(0)`, which can kill a background save that
/// is still pending. Waits for any in-flight write through the same lock. Skipping unchanged
/// settings keeps a hand-edited file that failed to load from being overwritten with defaults.
pub fn flush_settings(cx: &App) {
    let app = cx.global::<AppSettings>();
    let Some(path) = app.store.path.as_ref() else {
        return;
    };
    let _guard = app.write_lock.lock().unwrap_or_else(|e| e.into_inner());
    let current = app.generation.load(Ordering::SeqCst);
    if app.saved.load(Ordering::SeqCst) == current {
        return;
    }
    match SettingsStore::write(path, app.store.get()) {
        Ok(()) => app.saved.store(current, Ordering::SeqCst),
        Err(err) => log::warn!("saving settings to {} failed: {err}", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{UseDarkTheme, UseLightTheme};
    use crate::test_support::{exists, open_shell, press};
    use fdt_ui::theme::TokensExt as _;
    use fdt_ui::tokens::Appearance;
    use gpui_kit::TestAppContext;
    use gpui_kit::component::ActiveTheme as _;
    use gpui_kit::test::TestWindowExt as _;

    fn file_in(dir: &Path) -> PathBuf {
        dir.join("settings.json")
    }

    #[test]
    fn missing_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let store = SettingsStore::load(file_in(dir.path()));
        assert_eq!(store.get(), &Settings::default());
    }

    #[test]
    fn write_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = file_in(dir.path());
        let s = Settings {
            theme: ThemePreference::Dark,
            recent_commands: vec!["a".into(), "b".into()],
            free_app_http_memory: true,
            body_cap_kib: 64,
            body_budget_mib: 10,
            trace_widget_builds: true,
            recent_urls: vec!["ws://x/ws".into()],
        };
        SettingsStore::write(&path, &s).unwrap();
        assert_eq!(SettingsStore::load(path).get(), &s);
    }

    #[test]
    fn corrupt_file_gives_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = file_in(dir.path());
        std::fs::write(&path, "{not json").unwrap();
        assert_eq!(SettingsStore::load(path).get(), &Settings::default());
    }

    #[test]
    fn wrong_types_give_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = file_in(dir.path());
        std::fs::write(&path, r#"{"theme": 5}"#).unwrap();
        assert_eq!(SettingsStore::load(path).get(), &Settings::default());
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = file_in(dir.path());
        std::fs::write(&path, r#"{"theme":"dark","future":1}"#).unwrap();
        assert_eq!(SettingsStore::load(path).get().theme, ThemePreference::Dark);
    }

    #[test]
    fn one_bad_settings_field_keeps_the_others() {
        let dir = tempfile::tempdir().unwrap();
        let path = file_in(dir.path());
        std::fs::write(&path, r#"{"theme":"dark","recent_commands":5}"#).unwrap();
        let s = SettingsStore::load(path);
        assert_eq!(s.get().theme, ThemePreference::Dark);
        assert!(s.get().recent_commands.is_empty());
    }

    #[test]
    fn new_settings_fields_default() {
        let s = Settings::default();
        assert!(!s.free_app_http_memory);
        assert!(!s.trace_widget_builds);
        assert_eq!(s.body_cap_kib, 1024);
        assert_eq!(s.body_budget_mib, 200);
        assert!(s.recent_urls.is_empty());
        let dir = tempfile::tempdir().unwrap();
        let path = file_in(dir.path());
        std::fs::write(
            &path,
            r#"{"body_cap_kib":"big","recent_urls":["a"],"body_budget_mib":7}"#,
        )
        .unwrap();
        let s = SettingsStore::load(path);
        assert_eq!(s.get().body_cap_kib, 1024);
        assert_eq!(s.get().body_budget_mib, 7);
        assert_eq!(s.get().recent_urls, vec!["a".to_string()]);
    }

    #[test]
    fn recent_urls_dedupe_and_cap() {
        let mut v: Vec<String> = Vec::new();
        for u in ["a", "b", "c", "d", "e", "f"] {
            record_recent_url(&mut v, u);
        }
        assert_eq!(v, ["f", "e", "d", "c", "b"]);
        record_recent_url(&mut v, "d");
        assert_eq!(v, ["d", "f", "e", "c", "b"]);
    }

    #[test]
    fn write_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("settings.json");
        SettingsStore::write(&path, &Settings::default()).unwrap();
        assert!(path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn settings_are_written_for_the_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        SettingsStore::write(&path, &Settings::default()).unwrap();
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        // Writing again over an older, looser file tightens it.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        SettingsStore::write(&path, &Settings::default()).unwrap();
        assert_eq!(mode(&path), 0o600);
    }

    #[test]
    fn write_to_an_unwritable_path_errors() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file.txt");
        std::fs::write(&file, "x").unwrap();
        let path = file.join("settings.json");
        assert!(SettingsStore::write(&path, &Settings::default()).is_err());
    }

    #[gpui_kit::test]
    fn failed_save_keeps_the_app_running(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file.txt");
        std::fs::write(&file, "x").unwrap();
        cx.update(|cx| {
            init_settings(cx, SettingsStore::load(file.join("settings.json")));
            update_settings(cx, |s| s.theme = ThemePreference::Dark);
        });
        cx.run_until_parked();
        cx.update(|cx| assert_eq!(settings(cx).theme, ThemePreference::Dark));
    }

    #[gpui_kit::test]
    fn flush_writes_the_latest_settings_before_the_background_save_runs(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = file_in(dir.path());
        cx.update(|cx| {
            init_settings(cx, SettingsStore::load(path.clone()));
            update_settings(cx, |s| s.theme = ThemePreference::Light);
            // No `run_until_parked`: the background save has not run yet.
            flush_settings(cx);
        });
        assert_eq!(
            SettingsStore::load(path).get().theme,
            ThemePreference::Light
        );
    }

    #[gpui_kit::test]
    fn flush_leaves_the_file_alone_when_nothing_changed(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = file_in(dir.path());
        // A hand-edited file with a mistake: load falls back to defaults.
        std::fs::write(&path, "{not json").unwrap();
        cx.update(|cx| {
            init_settings(cx, SettingsStore::load(path.clone()));
            flush_settings(cx);
        });
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{not json");
    }

    #[gpui_kit::test]
    fn flush_skips_settings_the_background_save_already_wrote(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = file_in(dir.path());
        cx.update(|cx| {
            init_settings(cx, SettingsStore::load(path.clone()));
            update_settings(cx, |s| s.theme = ThemePreference::Dark);
        });
        cx.run_until_parked();
        std::fs::write(&path, "edited by hand").unwrap();
        cx.update(|cx| flush_settings(cx));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "edited by hand");
    }

    #[gpui_kit::test]
    fn flush_without_a_file_is_a_no_op(cx: &mut TestAppContext) {
        cx.update(|cx| {
            init_settings(cx, SettingsStore::in_memory());
            flush_settings(cx);
        });
    }

    #[gpui_kit::test]
    fn rapid_updates_leave_the_newest_settings_on_disk(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let path = file_in(dir.path());
        cx.update(|cx| {
            init_settings(cx, SettingsStore::load(path.clone()));
            for i in 0..20 {
                update_settings(cx, |s| {
                    s.recent_commands.push(format!("cmd-{i}"));
                    s.theme = if i % 2 == 0 {
                        ThemePreference::Dark
                    } else {
                        ThemePreference::Light
                    };
                });
            }
        });
        cx.executor().allow_parking();
        cx.run_until_parked();
        let on_disk = SettingsStore::load(path);
        cx.update(|cx| assert_eq!(on_disk.get(), settings(cx)));
        assert_eq!(on_disk.get().recent_commands.len(), 20);
    }

    #[gpui_kit::test]
    fn theme_actions_apply_and_persist_in_memory(cx: &mut TestAppContext) {
        let (_shell, cx) = open_shell(cx);
        cx.dispatch_action(UseDarkTheme);
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert!(cx.theme().is_dark());
            assert_eq!(settings(cx).theme, ThemePreference::Dark);
        });
        cx.dispatch_action(UseLightTheme);
        cx.run_until_parked();
        cx.update(|_, cx| {
            assert!(!cx.theme().is_dark());
            assert_eq!(settings(cx).theme, ThemePreference::Light);
        });
    }

    #[gpui_kit::test]
    fn system_preference_follows_os_but_pinned_does_not(cx: &mut TestAppContext) {
        let (shell, cx) = open_shell(cx);
        cx.update(|_, cx| assert_eq!(settings(cx).theme, ThemePreference::System));
        shell.update_in(cx, |s, w, cx| {
            s.appearance_changed(Appearance::Light, w, cx)
        });
        cx.update(|_, cx| assert_eq!(cx.appearance(), Appearance::Light));
        shell.update_in(cx, |s, w, cx| s.appearance_changed(Appearance::Dark, w, cx));
        cx.update(|_, cx| assert_eq!(cx.appearance(), Appearance::Dark));
        cx.dispatch_action(UseLightTheme);
        cx.run_until_parked();
        shell.update_in(cx, |s, w, cx| s.appearance_changed(Appearance::Dark, w, cx));
        cx.update(|_, cx| assert_eq!(cx.appearance(), Appearance::Light));
    }

    #[gpui_kit::test]
    fn settings_shortcut_opens_settings_and_back_returns(cx: &mut TestAppContext) {
        let (_shell, cx) = open_shell(cx);
        press(cx, "cmd-,", "ctrl-,");
        assert!(exists(cx, "settings-page"));
        press(cx, "cmd-[", "ctrl-[");
        assert!(exists(cx, "start-page"));
    }

    #[gpui_kit::test]
    fn clicking_theme_buttons_applies_theme(cx: &mut TestAppContext) {
        let (_shell, cx) = open_shell(cx);
        press(cx, "cmd-,", "ctrl-,");
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click("theme-light", cx);
        });
        cx.run_until_parked();
        cx.update(|_, cx| assert!(!cx.theme().is_dark()));
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.click("theme-dark", cx);
        });
        cx.run_until_parked();
        cx.update(|_, cx| assert!(cx.theme().is_dark()));
    }
}
