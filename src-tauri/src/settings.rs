//! Persisted user settings.
//!
//! Everything configurable used to be an environment variable read once at
//! startup, which is fine for development and unusable for anyone else. This
//! is a small JSON file in the data directory, written by the settings screen
//! and read at launch.
//!
//! Environment variables still win where they are set, so the development
//! overrides documented in the README keep working and can be used to
//! reproduce a report without editing someone's saved configuration.

use once_cell::sync::Lazy;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::hotkey::Chord;

/// The live settings, shared between the UI thread and the pipeline thread.
///
/// Read per-use rather than captured at construction, so changing the idle
/// timeout or turning off chimes takes effect without a restart. The hotkey
/// and the cleanup model are the exceptions: the first is pushed into the
/// keyboard hook when it changes, and the second needs a sidecar restart.
static CURRENT: Lazy<RwLock<Settings>> = Lazy::new(|| RwLock::new(Settings::default()));

/// A snapshot of the current settings.
pub fn current() -> Settings {
    CURRENT.read().clone()
}

/// Replace the live settings. Does not persist; call [`Settings::save`] too.
pub fn set_current(s: Settings) {
    *CURRENT.write() = s;
}

/// Where the overlay sits. It is click-through by design - a focusable overlay
/// would receive its own paste - so it cannot be dragged, and presets are the
/// honest alternative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum Overlay {
    /// Above the taskbar. The default, because dead centre is exactly where
    /// the text being dictated into tends to be.
    #[default]
    Bottom,
    Top,
    Centre,
    /// Do not show the overlay at all.
    Hidden,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// Push-to-talk chord, by key name: `["Ctrl", "Win"]`.
    pub hotkey: Vec<String>,
    pub overlay: Overlay,
    /// Seconds of inactivity before the microphone is released.
    pub idle_seconds: u64,
    /// Run the transcript through the local instruction model.
    pub cleanup: bool,
    /// Play the rising/falling chimes when a session opens and closes.
    pub chimes: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            hotkey: vec!["Ctrl".into(), "Win".into()],
            overlay: Overlay::default(),
            idle_seconds: 30,
            cleanup: true,
            chimes: true,
        }
    }
}

impl Settings {
    pub fn path() -> Option<PathBuf> {
        Some(crate::setup::install_root()?.join("settings.json"))
    }

    /// Read settings, falling back to defaults for anything missing or invalid.
    ///
    /// Never fails. A corrupt file gives defaults and a warning rather than
    /// refusing to start - being unable to dictate because a JSON file has a
    /// stray comma would be a poor trade.
    pub fn load() -> Self {
        let Some(path) = Self::path() else { return Self::default() };
        if !path.exists() {
            return Self::default();
        }
        match std::fs::read_to_string(&path).map(|s| serde_json::from_str::<Settings>(&s)) {
            Ok(Ok(s)) => {
                tracing::info!(path = %path.display(), "loaded settings");
                s
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, path = %path.display(), "settings file is invalid; using defaults");
                Self::default()
            }
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "could not read settings; using defaults");
                Self::default()
            }
        }
    }

    /// Write settings, replacing the file atomically.
    ///
    /// Via a temporary file and a rename so that losing power mid-write leaves
    /// the previous settings rather than a truncated file that would silently
    /// reset everything to defaults on next launch.
    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::path().ok_or_else(|| anyhow::anyhow!("no data directory"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)?;
        std::fs::rename(&tmp, &path)?;
        tracing::info!(path = %path.display(), "saved settings");
        Ok(())
    }

    /// The configured chord, or the default if the saved names are unusable.
    pub fn chord(&self) -> Chord {
        match Chord::from_names(&self.hotkey) {
            Some(c) => c,
            None => {
                tracing::warn!(names = ?self.hotkey, "unusable hotkey in settings; using default");
                Chord::default()
            }
        }
    }

    /// Effective idle timeout, with the environment override applied.
    pub fn effective_idle(&self) -> std::time::Duration {
        let secs = std::env::var("PARAFLOW_IDLE_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(self.idle_seconds)
            // A zero or absurd timeout would make the app unusable; clamp
            // rather than trust a hand-edited file.
            .clamp(5, 3600);
        std::time::Duration::from_secs(secs)
    }

    /// Whether the cleanup pass should run, with the environment override.
    pub fn effective_cleanup(&self) -> bool {
        if std::env::var("PARAFLOW_NO_CLEANUP").is_ok() {
            return false;
        }
        self.cleanup
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_shipped_behaviour() {
        let s = Settings::default();
        assert_eq!(s.chord().label(), "Ctrl + Win");
        assert_eq!(s.idle_seconds, 30);
        assert!(s.cleanup);
        assert_eq!(s.overlay, Overlay::Bottom);
    }

    #[test]
    fn a_partial_file_fills_in_the_rest() {
        // `#[serde(default)]` means someone can hand-edit one field without
        // having to write the whole structure.
        let s: Settings = serde_json::from_str(r#"{"idleSeconds": 90}"#).unwrap();
        assert_eq!(s.idle_seconds, 90);
        assert_eq!(s.chord().label(), "Ctrl + Win", "other fields keep defaults");
    }

    #[test]
    fn an_unusable_hotkey_falls_back_rather_than_becoming_unpressable() {
        let s = Settings { hotkey: vec!["Ctrl".into(), "Nonsense".into()], ..Default::default() };
        assert_eq!(s.chord().label(), "Ctrl + Win");
    }

    #[test]
    fn an_empty_hotkey_falls_back() {
        let s = Settings { hotkey: vec![], ..Default::default() };
        assert_eq!(s.chord().label(), "Ctrl + Win");
    }

    #[test]
    fn idle_timeout_is_clamped_to_something_usable() {
        let zero = Settings { idle_seconds: 0, ..Default::default() };
        assert_eq!(zero.effective_idle().as_secs(), 5, "zero would close the mic instantly");
        let huge = Settings { idle_seconds: 999_999, ..Default::default() };
        assert_eq!(huge.effective_idle().as_secs(), 3600);
    }

    #[test]
    fn settings_round_trip_through_json() {
        let original = Settings {
            hotkey: vec!["Ctrl".into(), "Shift".into(), "Space".into()],
            overlay: Overlay::Top,
            idle_seconds: 45,
            cleanup: false,
            chimes: false,
        };
        let text = serde_json::to_string(&original).unwrap();
        let back: Settings = serde_json::from_str(&text).unwrap();
        assert_eq!(back.chord().label(), "Ctrl + Shift + Space");
        assert_eq!(back.overlay, Overlay::Top);
        assert_eq!(back.idle_seconds, 45);
        assert!(!back.cleanup);
        assert!(!back.chimes);
    }
}
