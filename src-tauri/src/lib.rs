//! ParaFlow - local-first push-to-talk dictation for Windows.

mod asr;
mod audio;
mod hotkey;
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
use hotkey::{Chord, PttEvent};
use pipeline::Pipeline;
use polish::{Polish, PolishConfig};
use std::path::PathBuf;
use std::time::Duration;
use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Emitter, Manager,
};

/// Instruction model for the cleanup pass.
const DEFAULT_CLEANUP_MODEL: &str = "Qwen3-4B-Instruct-2507-Q4_K_M.gguf";
/// How long a dictation session stays live before the microphone is released.
const DEFAULT_IDLE_SECS: u64 = 30;

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
        .invoke_handler(tauri::generate_handler![setup_status, begin_setup])
        .setup(|app| {
            build_tray(app.handle())?;
            let handle = app.handle().clone();

            // Before deciding whether we are set up, throw away anything a
            // half-finished install left behind. Otherwise those files look
            // like a working runtime and the sidecar dies at first use.
            setup::purge_incomplete();

            if setup::ready(|p| resolve(p).is_ok()) {
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

/// Grow the always-on-top HUD into a focusable setup window.
fn show_setup_window(app: &tauri::AppHandle) {
    let Some(w) = app.get_webview_window("hud") else { return };
    let _ = w.set_size(tauri::LogicalSize::new(620.0_f64, 520.0_f64));
    let _ = w.set_always_on_top(false);
    let _ = w.center();
    let _ = w.show();
    let _ = w.set_focus();
}

/// Shrink back to the passive overlay once setup is done.
fn show_hud_window(app: &tauri::AppHandle) {
    let Some(w) = app.get_webview_window("hud") else { return };
    let _ = w.set_size(tauri::LogicalSize::new(420.0_f64, 132.0_f64));
    let _ = w.set_always_on_top(true);
    let _ = w.center();
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
    /// Download size in MB for each choice, on the detected backend.
    transcribe_mb: u64,
    polish_mb: u64,
}

#[tauri::command]
fn setup_status() -> SetupStatus {
    let accel = setup::Accel::detect();
    SetupStatus {
        needs_setup: !setup::ready(|p| resolve(p).is_ok()),
        accel,
        transcribe_mb: setup::download_mb(setup::Profile::Transcribe, accel),
        polish_mb: setup::download_mb(setup::Profile::Polish, accel),
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
    std::thread::Builder::new().name("paraflow-hotkey".into()).spawn(move || {
        if let Err(e) = hotkey::run(Chord::default(), tx) {
            tracing::error!(error = %e, "keyboard hook failed");
        }
    })?;

    let idle = Duration::from_secs(
        env_or("PARAFLOW_IDLE_SECS", &DEFAULT_IDLE_SECS.to_string())
            .parse()
            .unwrap_or(DEFAULT_IDLE_SECS),
    );

    // The microphone is not opened here - the first press does that.
    Pipeline::new(MicSession::new(), asr, polish, app, idle).run(rx);
    Ok(())
}

/// Bring up the cleanup sidecar, or carry on without it.
///
/// Dictation without cleanup is still useful, so nothing here is fatal: a
/// missing model or a failed launch downgrades to raw transcripts rather than
/// taking the app down.
fn start_polish() -> Option<Polish> {
    if std::env::var("PARAFLOW_NO_CLEANUP").is_ok() {
        tracing::info!("cleanup pass disabled by PARAFLOW_NO_CLEANUP");
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

fn build_tray(app: &tauri::AppHandle) -> Result<()> {
    let show = MenuItem::with_id(app, "show", "Show ParaFlow", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    TrayIconBuilder::with_id("paraflow-tray")
        .icon(app.default_window_icon().cloned().ok_or_else(|| anyhow!("no default icon"))?)
        .tooltip("ParaFlow - hold Ctrl+Win to dictate")
        .menu(&menu)
        .show_menu_on_left_click(true)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => {
                if let Some(w) = app.get_webview_window("hud") {
                    let _ = w.show();
                }
            }
            "quit" => app.exit(0),
            _ => {}
        })
        .build(app)?;

    Ok(())
}
