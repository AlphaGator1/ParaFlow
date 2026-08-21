//! FlowState - local-first push-to-talk dictation for Windows.

mod asr;
mod audio;
mod hotkey;
mod inject;
mod pipeline;
mod sidecar;
mod sound;

// Public so `examples/cleanup.rs` can exercise the real prompt rather than a
// copy of it. The cleanup prompt is the feature; a test harness that drifts
// from it would be worse than no harness.
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
    Manager,
};

/// Instruction model for the cleanup pass.
const DEFAULT_CLEANUP_MODEL: &str = "Qwen3-4B-Instruct-2507-Q4_K_M.gguf";
/// How long a dictation session stays live before the microphone is released.
const DEFAULT_IDLE_SECS: u64 = 30;

fn env_or(key: &str, fallback: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| fallback.to_string())
}

/// Locate a bundled resource. In development the vendor/ and models/ trees sit
/// beside src-tauri/; in a packaged build they are copied next to the exe.
fn resolve(relative: &str) -> Result<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            roots.push(dir.to_path_buf());
            // target/debug/ -> project root
            roots.push(dir.join("../../.."));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        roots.push(cwd.clone());
        roots.push(cwd.join(".."));
    }

    for root in &roots {
        let candidate = root.join(relative);
        if candidate.exists() {
            return Ok(candidate.canonicalize().unwrap_or(candidate));
        }
    }
    Err(anyhow!(
        "could not find `{relative}` in any of: {}",
        roots.iter().map(|r| r.display().to_string()).collect::<Vec<_>>().join(", ")
    ))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("FLOWSTATE_LOG")
                .unwrap_or_else(|_| "flowstate_lib=debug,info".into()),
        )
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            build_tray(app.handle())?;

            let handle = app.handle().clone();
            // The whole audio path lives on this one thread: cpal's Stream is
            // !Send on Windows, so the capture device must be opened on the
            // same thread that ultimately owns it.
            std::thread::Builder::new()
                .name("flowstate-pipeline".into())
                .spawn(move || {
                    if let Err(e) = start_pipeline(handle) {
                        tracing::error!(error = %e, "pipeline failed to start");
                    }
                })?;

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running FlowState");
}

fn start_pipeline(app: tauri::AppHandle) -> Result<()> {
    let backend = Backend::from_env();
    // Only whisper's CPU paths use these; leave the rest of the machine free
    // for whatever the user is actually working in.
    let config = Asr::locate(backend, std::env::var("FLOWSTATE_MODEL").ok(), resolve, 8)?;

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
    std::thread::Builder::new().name("flowstate-hotkey".into()).spawn(move || {
        if let Err(e) = hotkey::run(Chord::default(), tx) {
            tracing::error!(error = %e, "keyboard hook failed");
        }
    })?;

    let idle = Duration::from_secs(
        env_or("FLOWSTATE_IDLE_SECS", &DEFAULT_IDLE_SECS.to_string())
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
    if std::env::var("FLOWSTATE_NO_CLEANUP").is_ok() {
        tracing::info!("cleanup pass disabled by FLOWSTATE_NO_CLEANUP");
        return None;
    }

    let server_bin = match resolve("vendor/llama-cuda/llama-server.exe") {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "no llama-server; dictation will paste raw transcripts");
            return None;
        }
    };
    let model_name = env_or("FLOWSTATE_CLEANUP_MODEL", DEFAULT_CLEANUP_MODEL);
    let model = match resolve(&format!("models/{model_name}")) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "no cleanup model; dictation will paste raw transcripts");
            return None;
        }
    };

    match Polish::start(PolishConfig { server_bin, model, gpu_layers: 99, threads: 8 }) {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(error = %e, "cleanup sidecar failed to start; using raw transcripts");
            None
        }
    }
}

fn build_tray(app: &tauri::AppHandle) -> Result<()> {
    let show = MenuItem::with_id(app, "show", "Show FlowState", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &quit])?;

    TrayIconBuilder::with_id("flowstate-tray")
        .icon(app.default_window_icon().cloned().ok_or_else(|| anyhow!("no default icon"))?)
        .tooltip("FlowState - hold Ctrl+Win to dictate")
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
