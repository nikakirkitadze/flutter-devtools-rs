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

#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemePreference,
    pub recent_commands: Vec<String>,
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
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|err| {
                log::warn!("ignoring invalid settings file {}: {err}", path.display());
                Settings::default()
            }),
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
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)
    }
}

struct AppSettings {
    store: SettingsStore,
    /// Bumped on every update; a save only writes while it is still the latest.
    generation: Arc<AtomicU64>,
    /// Serialises writes so they never share the temporary file.
    write_lock: Arc<Mutex<()>>,
}

impl Global for AppSettings {}

pub fn init_settings(cx: &mut App, store: SettingsStore) {
    cx.set_global(AppSettings {
        store,
        generation: Arc::new(AtomicU64::new(0)),
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
    let lock = app.write_lock.clone();
    cx.background_executor()
        .spawn(async move {
            let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            // A newer update has queued its own write; skip this stale one.
            if latest.load(Ordering::SeqCst) != generation {
                return;
            }
            if let Err(err) = SettingsStore::write(&path, &snapshot) {
                log::warn!("saving settings to {} failed: {err}", path.display());
            }
        })
        .detach();
}

/// Writes the current settings to disk now, on the calling thread.
///
/// Used when the app quits: macOS then calls `exit(0)`, which can kill a background save that
/// is still pending. Waits for any in-flight write through the same lock.
pub fn flush_settings(cx: &App) {
    let app = cx.global::<AppSettings>();
    let Some(path) = app.store.path.as_ref() else {
        return;
    };
    let _guard = app.write_lock.lock().unwrap_or_else(|e| e.into_inner());
    if let Err(err) = SettingsStore::write(path, app.store.get()) {
        log::warn!("saving settings to {} failed: {err}", path.display());
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
    fn write_creates_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a").join("b").join("settings.json");
        SettingsStore::write(&path, &Settings::default()).unwrap();
        assert!(path.exists());
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
        assert!(exists(cx, "tool-page-timeline"));
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
