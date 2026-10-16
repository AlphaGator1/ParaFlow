//! ParaFlow - local-first push-to-talk dictation for Windows.

mod asr;
mod audio;
mod hotkey;
mod settings;
mod setup;
mod inject;
mod pipeline;
mod sidecar;
mod sound;

// Public so `examples/cleanup.rs` can exercise the real prompt rather than a
// copy of it. The cleanup prompt is the feature; a test harness that drifts
// from it would be worse than no harness.
pub mod dictionary;
pub mod polish;

use anyhow::{anyhow, Result};
use asr::{Asr, Backend};
use audio::MicSession;
use hotkey::PttEvent;
use pipeline::Pipeline;
use polish::{Polish, PolishConfig};
use std::path::PathBuf;
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Emitter, Manager,
};

/// Instruction model for the cleanup pass.
const DEFAULT_CLEANUP_MODEL: &str = "Qwen3-4B-Instruct-2507-Q4_K_M.gguf";

/// The user guide, as it is named beside the installed executable. Bundled as
/// a Tauri resource; see `bundle.resources` in `tauri.conf.json`.
const GUIDE_FILE: &str = "ParaFlow User Guide.html";

fn env_or(key: &str, fallback: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| fallback.to_string())
}

/// Locate a runtime binary or model file.
///
/// Installed assets win over the repo. An end user has everything under
/// `%LOCALAPPDATA%\ParaFlow`, fetched on first run; a developer has `vendor/`
/// and `models/` checked out beside `src-tauri/`. Looking at the install root
/// first means a developer who has also run the installer still gets their
/// working tree, because `vendor/` only exists in the latter case.
///
/// Paths are written repo-style (`vendor/parakeet-cuda/...`); the `vendor/`
/// prefix maps to `runtimes/` under the install root.
fn resolve(relative: &str) -> Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Some(root) = setup::install_root() {
        candidates.push(root.join(relative.replacen("vendor/", "runtimes/", 1)));
    }

    let mut roots: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            roots.push(dir.to_path_buf());
            // target/release/ -> project root
            roots.push(dir.join("../../.."));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd.clone());
        roots.push(cwd.join(".."));
    }
    candidates.extend(roots.iter().map(|r| r.join(relative)));

    for candidate in &candidates {
        if candidate.exists() {
            return Ok(candidate.canonicalize().unwrap_or_else(|_| candidate.clone()));
        }
    }
    Err(anyhow!(
        "could not find `{relative}` in any of: {}",
        candidates.iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join(", ")
    ))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("PARAFLOW_LOG")
                .unwrap_or_else(|_| "paraflow_lib=debug,info".into()),
        )
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            setup_status,
            begin_setup,
            get_settings,
            save_settings,
            start_hotkey_capture,
            cancel_hotkey_capture,
            open_dictionary_file,
            open_user_guide,
        ])
        .setup(|app| {
            // Load before anything reads it, so the hook is installed with the
            // user's chord rather than the default and then corrected.
            settings::set_current(settings::Settings::load());

            build_tray(app.handle())?;
            let handle = app.handle().clone();

            // Before deciding whether we are set up, throw away anything a
            // half-finished install left behind. Otherwise those files look
            // like a working runtime and the sidecar dies at first use.
            setup::purge_incomplete();

            if setup::ready(|p| resolve(p).is_ok()) {
                set_hud_click_through(&handle, true);
                apply_overlay(&handle, settings::current().overlay);
                spawn_pipeline(handle);
            } else {
                // Nothing to dictate with yet. Grow the HUD into a real window
                // and let the setup screen drive; the pipeline starts once
                // `begin_setup` finishes.
                tracing::info!("no recogniser found; showing first-run setup");
                show_setup_window(&handle);
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running ParaFlow");
}

/// Let the mouse through the overlay, or not.
///
/// The HUD has always *looked* click-through - `pointer-events: none` on the
/// pill - but that is a CSS rule about elements inside the webview and says
/// nothing about whether Windows routes clicks to the window. Without
/// `set_ignore_cursor_events` the overlay is a 420x132 always-on-top rectangle
/// quietly eating clicks near the bottom of the screen. The setup screen lives
/// in this same window and does need the mouse, so it is a mode, not a
/// constant.
fn set_hud_click_through(app: &tauri::AppHandle, through: bool) {
    let Some(w) = app.get_webview_window("hud") else { return };
    if let Err(e) = w.set_ignore_cursor_events(through) {
        tracing::warn!(error = %e, through, "could not set overlay click-through");
    }
}

/// Grow the always-on-top HUD into a focusable setup window.
fn show_setup_window(app: &tauri::AppHandle) {
    let Some(w) = app.get_webview_window("hud") else { return };
    // Setup is a real form with real buttons; it must receive the mouse.
    set_hud_click_through(app, false);
    let _ = w.set_size(tauri::LogicalSize::new(620.0_f64, 520.0_f64));
    let _ = w.set_always_on_top(false);
    let _ = w.center();
    let _ = w.show();
    let _ = w.set_focus();
}

/// Where the HUD sits on screen.
///
/// It is click-through by design - a focusable overlay would receive its own
/// paste - so it cannot be dragged. Presets are the honest alternative: they
/// keep the overlay passive while still letting people move it off whatever it
/// happens to be covering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HudSpot {
    Bottom,
    Top,
    Centre,
}

impl HudSpot {
    fn from_id(id: &str) -> Option<Self> {
        match id {
            "pos-bottom" => Some(HudSpot::Bottom),
            "pos-top" => Some(HudSpot::Top),
            "pos-centre" => Some(HudSpot::Centre),
            _ => None,
        }
    }

    fn as_setting(self) -> settings::Overlay {
        match self {
            HudSpot::Bottom => settings::Overlay::Bottom,
            HudSpot::Top => settings::Overlay::Top,
            HudSpot::Centre => settings::Overlay::Centre,
        }
    }
}

/// Place the HUD, defaulting to bottom-centre.
///
/// Dead centre is the worst possible spot for a passive overlay: it is exactly
/// where the text being dictated into tends to be.
fn place_hud(app: &tauri::AppHandle, spot: HudSpot) {
    let Some(w) = app.get_webview_window("hud") else { return };
    let Ok(Some(monitor)) = w.current_monitor() else {
        let _ = w.center();
        return;
    };

    let scale = monitor.scale_factor();
    let screen = monitor.size().to_logical::<f64>(scale);
    let pos = monitor.position().to_logical::<f64>(scale);
    let (win_w, win_h) = (420.0_f64, 132.0_f64);
    let margin = 48.0_f64;

    let x = pos.x + (screen.width - win_w) / 2.0;
    let y = match spot {
        // Above the taskbar, out of the way of whatever is being typed into.
        HudSpot::Bottom => pos.y + screen.height - win_h - margin,
        HudSpot::Top => pos.y + margin,
        HudSpot::Centre => pos.y + (screen.height - win_h) / 2.0,
    };
    let _ = w.set_position(tauri::LogicalPosition::new(x, y));
}

/// Shrink back to the passive overlay once setup is done.
fn show_hud_window(app: &tauri::AppHandle) {
    let Some(w) = app.get_webview_window("hud") else { return };
    let _ = w.set_size(tauri::LogicalSize::new(420.0_f64, 132.0_f64));
    let _ = w.set_always_on_top(true);
    set_hud_click_through(app, true);
    place_hud(app, HudSpot::Bottom);
}

/// True once the pipeline thread has been started, so setup completing twice
/// cannot bring up two sets of sidecars.
static PIPELINE_STARTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn spawn_pipeline(handle: tauri::AppHandle) {
    use std::sync::atomic::Ordering;
    if PIPELINE_STARTED.swap(true, Ordering::SeqCst) {
        tracing::warn!("pipeline already running; ignoring duplicate start");
        return;
    }
    // Say so before the slow part begins. Loading two models onto the GPU takes
    // seconds, the keyboard hook is not installed until they are up, and the
    // overlay used to spend that whole window claiming to be ready - so the
    // first press of a fresh launch vanished with no chime and no explanation.
    let _ = handle.emit("flow://stage", pipeline::Stage::Starting);

    // The whole audio path lives on this one thread: cpal's Stream is !Send on
    // Windows, so the capture device must be opened on the same thread that
    // ultimately owns it.
    let _ = std::thread::Builder::new().name("paraflow-pipeline".into()).spawn(move || {
        if let Err(e) = start_pipeline(handle.clone()) {
            tracing::error!(error = %e, "pipeline failed to start");
            let _ = handle.emit("paraflow://fatal", e.to_string());
        }
    });
}

/// What the setup screen needs to render its choices.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct SetupStatus {
    /// True when nothing is installed and the setup screen should be shown.
    needs_setup: bool,
    /// Which GPU backend was detected for this machine.
    accel: setup::Accel,
    /// Full download size in MB for each choice, on the detected backend.
    transcribe_mb: u64,
    polish_mb: u64,
    /// What is actually left to fetch, once anything already on disk is
    /// discounted. Lower than the total when resuming or changing profile.
    transcribe_remaining_mb: u64,
    polish_remaining_mb: u64,
}

#[tauri::command]
fn setup_status() -> SetupStatus {
    let accel = setup::Accel::detect();
    SetupStatus {
        needs_setup: !setup::ready(|p| resolve(p).is_ok()),
        accel,
        transcribe_mb: setup::download_mb(setup::Profile::Transcribe, accel),
        polish_mb: setup::download_mb(setup::Profile::Polish, accel),
        transcribe_remaining_mb: setup::remaining_mb(setup::Profile::Transcribe, accel),
        polish_remaining_mb: setup::remaining_mb(setup::Profile::Polish, accel),
    }
}

/// Download and install the chosen profile, then start dictating.
#[tauri::command]
fn begin_setup(app: tauri::AppHandle, profile: setup::Profile) -> Result<(), String> {
    let accel = setup::Accel::detect();
    std::thread::Builder::new()
        .name("paraflow-setup".into())
        .spawn(move || {
            let emitter = app.clone();
            let result = setup::install(profile, accel, move |p| {
                let _ = emitter.emit("paraflow://setup-progress", p);
            });
            match result {
                Ok(root) => {
                    tracing::info!(root = %root.display(), "setup complete");
                    let _ = app.emit("paraflow://setup-done", ());
                    show_hud_window(&app);
                    // Setup runs once, and this is the moment someone has the
                    // most questions and the least idea that a tray icon is
                    // where the answers live. Everything after this is opt-in.
                    open_guide(&app);
                    spawn_pipeline(app.clone());
                }
                Err(e) => {
                    tracing::error!(error = %e, "setup failed");
                    let _ = app.emit("paraflow://setup-failed", e.to_string());
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn start_pipeline(app: tauri::AppHandle) -> Result<()> {
    let backend = Backend::from_env();
    // Only whisper's CPU paths use these; leave the rest of the machine free
    // for whatever the user is actually working in.
    let config = Asr::locate(backend, std::env::var("PARAFLOW_MODEL").ok(), resolve, 8)?;

    tracing::info!(
        ?backend,
        server = %config.server_bin.display(),
        gpu = config.gpu,
        "resolved ASR backend"
    );

    let asr = Asr::start(config)?;

    let polish = start_polish();

    // The hook thread must do nothing but forward events, so give it a channel
    // rather than any real work.
    let (tx, rx) = crossbeam_channel::unbounded::<PttEvent>();
    let current = settings::current();
    let chord = current.chord();
    let hands_free = current.effective_hands_free();
    tracing::info!(hotkey = %chord.label(), hands_free, "installing push-to-talk chord");
    std::thread::Builder::new().name("paraflow-hotkey".into()).spawn(move || {
        if let Err(e) = hotkey::run(chord, hands_free, tx) {
            tracing::error!(error = %e, "keyboard hook failed");
        }
    })?;

    // The microphone is not opened here - the first press does that.
    Pipeline::new(MicSession::new(), asr, polish, app).run(rx);
    Ok(())
}

// --- settings commands ------------------------------------------------------

#[tauri::command]
fn get_settings() -> settings::Settings {
    settings::current()
}

/// Persist settings and apply whatever can be applied without a restart.
#[tauri::command]
fn save_settings(app: tauri::AppHandle, next: settings::Settings) -> Result<(), String> {
    let previous = settings::current();

    next.save().map_err(|e| e.to_string())?;
    settings::set_current(next.clone());

    // The hook holds its own copy of the chord, so push the new one in.
    if next.hotkey != previous.hotkey {
        hotkey::set_chord(next.chord());
        if let Some(tray) = app.tray_by_id(TRAY_ID) {
            let _ = tray.set_tooltip(Some(tray_tooltip(&next.chord())));
        }
    }
    // Same for the latch: the double-press test happens on the hook thread,
    // so switching it off has to reach the hook rather than only the settings.
    if next.hands_free != previous.hands_free {
        hotkey::set_latch_enabled(next.effective_hands_free());
    }
    // The overlay prints the chord in its idle hint, so it has to be told -
    // otherwise it keeps advertising the old keys while the new ones work,
    // which reads as "the change did not take".
    let _ = app.emit("paraflow://settings-changed", next.clone());
    // Overlay position and visibility are ours to apply immediately.
    apply_overlay(&app, next.overlay);

    // Turning cleanup on or off means starting or stopping a sidecar holding
    // gigabytes of VRAM, which cannot be done from under the pipeline thread.
    if next.cleanup != previous.cleanup {
        let _ = app.emit(
            "paraflow://notice",
            "Restart ParaFlow to apply the cleanup change.",
        );
    }
    Ok(())
}

/// Begin reading a chord for the settings screen.
///
/// While this is active every keystroke is swallowed, so the capture field can
/// see combinations like Ctrl+Alt+D without them reaching whatever is behind
/// the window.
#[tauri::command]
fn start_hotkey_capture(app: tauri::AppHandle) {
    let (tx, rx) = crossbeam_channel::unbounded::<hotkey::CaptureEvent>();
    hotkey::begin_capture(tx);

    std::thread::Builder::new()
        .name("paraflow-capture".into())
        .spawn(move || {
            for event in rx.iter() {
                match event {
                    hotkey::CaptureEvent::Preview(keys) => {
                        let names: Vec<String> = keys.iter().map(|k| hotkey::vk_name(*k)).collect();
                        match app.emit("paraflow://capture-preview", names.clone()) {
                            Ok(()) => tracing::debug!(?names, "emitted capture preview"),
                            Err(e) => tracing::warn!(error = %e, "capture preview emit failed"),
                        }
                    }
                    hotkey::CaptureEvent::Done(keys) => {
                        let names: Vec<String> = keys.iter().map(|k| hotkey::vk_name(*k)).collect();
                        let _ = app.emit("paraflow://capture-done", names);
                        return;
                    }
                }
            }
        })
        .ok();
}

#[tauri::command]
fn cancel_hotkey_capture() {
    hotkey::cancel_capture();
}

#[tauri::command]
fn open_dictionary_file(app: tauri::AppHandle) {
    open_dictionary(&app);
}

#[tauri::command]
fn open_user_guide(app: tauri::AppHandle) {
    open_guide(&app);
}

/// Move or hide the overlay to match the setting.
fn apply_overlay(app: &tauri::AppHandle, overlay: settings::Overlay) {
    let Some(w) = app.get_webview_window("hud") else { return };
    match overlay {
        settings::Overlay::Hidden => {
            let _ = w.hide();
        }
        settings::Overlay::Bottom => {
            let _ = w.show();
            place_hud(app, HudSpot::Bottom);
        }
        settings::Overlay::Top => {
            let _ = w.show();
            place_hud(app, HudSpot::Top);
        }
        settings::Overlay::Centre => {
            let _ = w.show();
            place_hud(app, HudSpot::Centre);
        }
    }
}

/// Open (or focus) the settings window.
///
/// A separate window rather than a mode of the HUD: the HUD is click-through
/// and never focused, because an overlay that takes focus receives its own
/// paste. Settings needs both.
fn open_settings_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
        return;
    }
    match tauri::WebviewWindowBuilder::new(
        app,
        "settings",
        tauri::WebviewUrl::App("index.html?view=settings".into()),
    )
    .title("ParaFlow Settings")
    .inner_size(560.0, 640.0)
    .resizable(true)
    .center()
    .build()
    {
        Ok(w) => {
            let _ = w.set_focus();
        }
        Err(e) => tracing::error!(error = %e, "could not open settings window"),
    }
}

/// Bring up the cleanup sidecar, or carry on without it.
///
/// Dictation without cleanup is still useful, so nothing here is fatal: a
/// missing model or a failed launch downgrades to raw transcripts rather than
/// taking the app down.
fn start_polish() -> Option<Polish> {
    if !settings::current().effective_cleanup() {
        tracing::info!("cleanup pass disabled in settings");
        return None;
    }

    // CUDA first for the same reason as the recogniser, but it matters far
    // more here: Vulkan runs the 4B cleanup model ~9x slower.
    let server_bin = match resolve("vendor/llama-cuda/llama-server.exe")
        .or_else(|_| resolve("vendor/llama-vulkan/llama-server.exe"))
    {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "no llama-server; dictation will paste raw transcripts");
            return None;
        }
    };
    let model_name = env_or("PARAFLOW_CLEANUP_MODEL", DEFAULT_CLEANUP_MODEL);
    let model = match resolve(&format!("models/{model_name}")) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "no cleanup model; dictation will paste raw transcripts");
            return None;
        }
    };

    let dictionary = dictionary::Dictionary::load();

    match Polish::start(PolishConfig {
        server_bin,
        model,
        gpu_layers: 99,
        threads: 8,
        dictionary,
    }) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(error = %e, "cleanup sidecar failed to start; using raw transcripts");
            None
        }
    }
}

const TRAY_ID: &str = "paraflow-tray";

/// The tray's hover text. Built from the live chord, never written out, for
/// the same reason the overlay reads it from settings: a hint naming the old
/// keys after a rebind reads as "the change did not take".
fn tray_tooltip(chord: &hotkey::Chord) -> String {
    format!("ParaFlow - hold {} to dictate", chord.label())
}

fn build_tray(app: &tauri::AppHandle) -> Result<()> {
    use tauri::menu::{PredefinedMenuItem, Submenu};

    let settings_item =
        MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?;
    let show = MenuItem::with_id(app, "show", "Show overlay", true, None::<&str>)?;

    let pos = Submenu::with_items(
        app,
        "Overlay position",
        true,
        &[
            &MenuItem::with_id(app, "pos-bottom", "Bottom", true, None::<&str>)?,
            &MenuItem::with_id(app, "pos-top", "Top", true, None::<&str>)?,
            &MenuItem::with_id(app, "pos-centre", "Centre", true, None::<&str>)?,
        ],
    )?;

    let dict = MenuItem::with_id(app, "dictionary", "Edit personal dictionary…", true, None::<&str>)?;
    let folder = MenuItem::with_id(app, "folder", "Open data folder", true, None::<&str>)?;
    let guide = MenuItem::with_id(app, "guide", "User guide", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit ParaFlow", true, None::<&str>)?;

    let menu = Menu::with_items(
        app,
        &[
            &settings_item,
            &PredefinedMenuItem::separator(app)?,
            &show,
            &pos,
            &PredefinedMenuItem::separator(app)?,
            &dict,
            &folder,
            &PredefinedMenuItem::separator(app)?,
            &guide,
            &PredefinedMenuItem::separator(app)?,
            &quit,
        ],
    )?;

    TrayIconBuilder::with_id(TRAY_ID)
        .icon(app.default_window_icon().cloned().ok_or_else(|| anyhow!("no default icon"))?)
        .tooltip(tray_tooltip(&settings::current().chord()))
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| {
            let id = event.id.as_ref();
            if let Some(spot) = HudSpot::from_id(id) {
                place_hud(app, spot);
                // Persist, so the choice survives a restart rather than
                // silently reverting and looking broken.
                let mut s = settings::current();
                s.overlay = spot.as_setting();
                settings::set_current(s.clone());
                if let Err(e) = s.save() {
                    tracing::warn!(error = %e, "could not save overlay position");
                }
                return;
            }
            match id {
                "settings" => open_settings_window(app),
                "show" => {
                    if let Some(w) = app.get_webview_window("hud") {
                        let _ = w.show();
                    }
                }
                // Opening the file in the user's default editor is a real
                // feature, not a placeholder: the dictionary is a commented
                // plain-text list precisely so it can be edited this way.
                "dictionary" => open_dictionary(app),
                "folder" => {
                    if let Some(root) = setup::install_root() {
                        let _ = tauri_plugin_opener::open_path(root, None::<&str>);
                    }
                }
                "guide" => open_guide(app),
                "quit" => app.exit(0),
                _ => {}
            }
        })
        .build(app)?;

    Ok(())
}

/// Locate the user guide.
///
/// Installed builds have it beside `paraflow.exe`; a developer running from
/// the repo has `docs/user-guide.html`, which [`resolve`] already knows how to
/// find from either the target directory or the working directory.
fn guide_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    if let Ok(dir) = app.path().resource_dir() {
        let installed = dir.join(GUIDE_FILE);
        if installed.exists() {
            return Some(installed);
        }
    }
    resolve("docs/user-guide.html").ok()
}

/// Show the user guide in the default browser.
fn open_guide(app: &tauri::AppHandle) {
    let Some(path) = guide_path(app) else {
        tracing::warn!("user guide not found beside the executable or in the repo");
        let _ = app.emit("paraflow://notice", "Could not find the user guide.");
        return;
    };
    if let Err(e) = tauri_plugin_opener::open_path(&path, None::<&str>) {
        tracing::warn!(error = %e, path = %path.display(), "could not open user guide");
        let _ = app.emit("paraflow://notice", "Could not open the user guide.");
    }
}

/// Open the personal dictionary for editing, creating it if absent.
fn open_dictionary(app: &tauri::AppHandle) {
    let Some(path) = dictionary::Dictionary::path() else { return };
    if !path.exists() {
        // Loading writes the commented template on first use.
        let _ = dictionary::Dictionary::load();
    }
    if let Err(e) = tauri_plugin_opener::open_path(&path, None::<&str>) {
        tracing::warn!(error = %e, path = %path.display(), "could not open dictionary");
        return;
    }
    // Terms are read once at startup so they stay inside the prompt cache, so
    // an edit made now will not apply until ParaFlow restarts. Say so rather
    // than let someone wonder why their new word is ignored.
    let _ = app.emit("paraflow://notice", "Dictionary opened. Restart ParaFlow to apply changes.");
}
