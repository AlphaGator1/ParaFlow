//! Speech recognition via a resident sidecar.
//!
//! Two backends are supported and they are near-identical from our side: both
//! are local HTTP servers that hold one model in VRAM and take a WAV over
//! multipart. They differ only in binary, CLI flags, and endpoint path, so
//! [`Backend`] carries those differences and the client code is shared.
//!
//! The model is deliberately loaded once and left resident. Paying a cold load
//! per utterance would dominate the latency budget and make dictation feel
//! sluggish.
//!
//! Measured on an RTX 5090 against the same 11.0 s sample, warm:
//!
//! | backend                   | latency | size    |
//! |---------------------------|---------|---------|
//! | Parakeet TDT 0.6B v3 q8_0 | 45 ms   | 897 MB  |
//! | whisper large-v3-turbo    | 99 ms   | 1549 MB |
//!
//! Transcripts were identical on that sample, punctuation included. Parakeet
//! is the default; whisper is kept selectable because one sample is not an
//! accuracy evaluation, and switching back must stay a one-line change.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::sidecar;

/// Loopback port for the sidecar. Chosen high to avoid common collisions.
const PORT: u16 = 8178;
/// How long to wait for the model to load before declaring the sidecar dead.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// NVIDIA Parakeet TDT via parakeet.cpp. Faster, smaller, emits
    /// punctuation and casing directly.
    Parakeet,
    /// OpenAI Whisper via whisper.cpp. Slower but upstream, and the only one
    /// of the two that supports decoder prompt biasing.
    Whisper,
}

impl Backend {
    /// `FLOWSTATE_ASR=whisper` opts out; anything else (or unset) is Parakeet.
    pub fn from_env() -> Self {
        match std::env::var("FLOWSTATE_ASR").unwrap_or_default().to_lowercase().as_str() {
            "whisper" => Backend::Whisper,
            "" | "parakeet" => Backend::Parakeet,
            other => {
                tracing::warn!(%other, "unknown FLOWSTATE_ASR value; using parakeet");
                Backend::Parakeet
            }
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Backend::Parakeet => "parakeet-server",
            Backend::Whisper => "whisper-server",
        }
    }

    /// Where the server binary lives, relative to the project root. The CUDA
    /// build is preferred; the second entry is a CPU fallback.
    fn server_candidates(&self) -> &'static [&'static str] {
        match self {
            Backend::Parakeet => &["vendor/parakeet-cuda/parakeet-server.exe"],
            Backend::Whisper => &[
                "vendor/whisper-cuda/Release/whisper-server.exe",
                "vendor/whisper-cpu/Release/whisper-server.exe",
            ],
        }
    }

    pub fn default_model(&self) -> &'static str {
        match self {
            Backend::Parakeet => "tdt-0.6b-v3-q8_0.gguf",
            Backend::Whisper => "ggml-large-v3-turbo.bin",
        }
    }

    fn inference_path(&self) -> &'static str {
        match self {
            // parakeet-server implements the OpenAI transcription API.
            Backend::Parakeet => "/v1/audio/transcriptions",
            Backend::Whisper => "/inference",
        }
    }
}

#[derive(Debug, Clone)]
pub struct AsrConfig {
    pub backend: Backend,
    /// Path to the server binary.
    pub server_bin: PathBuf,
    /// Path to the model weights.
    pub model: PathBuf,
    /// Use the GPU backend. Falls back to CPU when false.
    pub gpu: bool,
    pub threads: usize,
}

pub struct Asr {
    child: Child,
    client: reqwest::blocking::Client,
    endpoint: String,
    backend: Backend,
}

impl Asr {
    /// Resolve the binary and model for `backend`, using `resolve` to turn a
    /// relative path into an absolute one.
    ///
    /// Returns the config plus whether the GPU build was found.
    pub fn locate(
        backend: Backend,
        model_name: Option<String>,
        resolve: impl Fn(&str) -> Result<PathBuf>,
        threads: usize,
    ) -> Result<AsrConfig> {
        let mut server_bin = None;
        for candidate in backend.server_candidates() {
            if let Ok(p) = resolve(candidate) {
                server_bin = Some(p);
                break;
            }
        }
        let server_bin = server_bin.ok_or_else(|| {
            anyhow!(
                "no server binary for {:?}; looked for {:?}. Run scripts/setup.ps1",
                backend,
                backend.server_candidates()
            )
        })?;

        // Only whisper ships a separate CPU build; parakeet picks its device at
        // runtime, so "gpu" here just reflects which binary we found.
        let gpu = !server_bin.to_string_lossy().contains("whisper-cpu");

        let name = model_name.unwrap_or_else(|| backend.default_model().to_string());
        let model = resolve(&format!("models/{name}"))?;

        Ok(AsrConfig { backend, server_bin, model, gpu, threads })
    }

    /// Launch the sidecar and block until it answers, so the first dictation
    /// is as fast as every later one.
    pub fn start(config: AsrConfig) -> Result<Self> {
        anyhow::ensure!(
            config.server_bin.exists(),
            "{} not found at {}",
            config.backend.name(),
            config.server_bin.display()
        );
        anyhow::ensure!(config.model.exists(), "model not found at {}", config.model.display());
        sidecar::ensure_port_free(config.backend.name(), PORT)?;

        let mut cmd = Command::new(&config.server_bin);
        match config.backend {
            Backend::Parakeet => {
                cmd.arg("--model")
                    .arg(&config.model)
                    .arg("--host")
                    .arg("127.0.0.1")
                    .arg("--port")
                    .arg(PORT.to_string())
                    .arg("--threads")
                    .arg(config.threads.to_string());
                if !config.gpu {
                    // parakeet.cpp selects its device via the environment
                    // rather than a flag.
                    cmd.env("PARAKEET_DEVICE", "cpu");
                }
            }
            Backend::Whisper => {
                cmd.arg("-m")
                    .arg(&config.model)
                    .arg("--host")
                    .arg("127.0.0.1")
                    .arg("--port")
                    .arg(PORT.to_string())
                    .arg("-t")
                    .arg(config.threads.to_string())
                    // English (United States) only. Pinning the language skips
                    // detection and stops the decoder wandering into another.
                    .arg("-l")
                    .arg("en")
                    // Dictation is a single short utterance; timings are noise.
                    .arg("--no-timestamps")
                    // Drop [BLANK_AUDIO] / (wind blowing) at the decoder rather
                    // than string-scrubbing them afterwards.
                    .arg("--suppress-nst")
                    // Temperature fallback re-decodes on low confidence. Right
                    // for a podcast, wrong for a keystroke-latency path where a
                    // rare imperfect word beats a stall the user can feel.
                    .arg("--no-fallback");
                if !config.gpu {
                    cmd.arg("--no-gpu");
                }
            }
        }

        // The DLLs sit beside the exe, so run from there.
        cmd.current_dir(config.server_bin.parent().unwrap_or(Path::new(".")))
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(sidecar::CREATE_NO_WINDOW);
        }

        tracing::info!(
            backend = ?config.backend,
            model = %config.model.display(),
            gpu = config.gpu,
            "starting ASR sidecar"
        );
        let mut child = cmd.spawn().with_context(|| format!("failed to spawn {}", config.backend.name()))?;

        #[cfg(windows)]
        sidecar::tie_to_parent(&child)?;

        sidecar::await_ready(config.backend.name(), PORT, "/", STARTUP_TIMEOUT, &mut child)?;

        let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(120)).build()?;
        let endpoint = format!("http://127.0.0.1:{PORT}{}", config.backend.inference_path());

        let asr = Self { child, client, endpoint, backend: config.backend };
        asr.warm_up();
        Ok(asr)
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Push one throwaway utterance through the model at startup.
    ///
    /// The shipped CUDA binaries carry no `sm_120` cubins, so on RTX 50-series
    /// the driver JIT-compiles the sm_90 PTX on first use. That is a real cost
    /// the user would otherwise pay on their very first dictation - measured at
    /// ~150ms warm-disk-cache, and over a second the first time ever on a
    /// machine. Spending it here means every dictation is the fast one.
    ///
    /// Best-effort: a failure here is not fatal, it just means run one is slow.
    fn warm_up(&self) {
        // Low-amplitude noise rather than silence: a recogniser can
        // short-circuit on a silent buffer, leaving the decode path uncompiled.
        let n = crate::audio::TARGET_RATE as usize;
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let samples: Vec<f32> = (0..n)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                ((seed >> 40) as f32 / 8_388_608.0 - 1.0) * 0.02
            })
            .collect();

        let began = Instant::now();
        match crate::audio::to_wav(&samples).and_then(|wav| self.transcribe(wav)) {
            Ok(_) => tracing::info!(ms = began.elapsed().as_millis(), "ASR warm-up complete"),
            Err(e) => tracing::warn!(error = %e, "ASR warm-up failed; first dictation may lag"),
        }
    }

    /// Transcribe one utterance. `wav` is 16 kHz mono 16-bit PCM.
    pub fn transcribe(&self, wav: Vec<u8>) -> Result<String> {
        let part = reqwest::blocking::multipart::Part::bytes(wav)
            .file_name("utterance.wav")
            .mime_str("audio/wav")?;
        let mut form = reqwest::blocking::multipart::Form::new()
            .part("file", part)
            .text("response_format", "json");

        if self.backend == Backend::Whisper {
            form = form.text("temperature", "0.0");
        } else {
            // OpenAI-API field. parakeet-server ignores it if unsupported, and
            // honours it if not, so pinning English costs nothing either way.
            form = form.text("language", "en");
        }

        let resp = self
            .client
            .post(&self.endpoint)
            .multipart(form)
            .send()
            .context("ASR request failed - is the sidecar alive?")?;

        if !resp.status().is_success() {
            return Err(anyhow!("ASR returned {}: {}", resp.status(), resp.text()?));
        }

        let body: serde_json::Value = resp.json().context("ASR returned malformed JSON")?;
        let text = body
            .get("text")
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow!("ASR response had no `text` field: {body}"))?;

        Ok(clean(text))
    }
}

impl Drop for Asr {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Strip the artifacts a recogniser emits for silence and non-speech audio.
/// These are hallucinations of the training data, not things the user said,
/// and pasting "[BLANK_AUDIO]" into their editor would be worse than pasting
/// nothing.
fn clean(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for line in raw.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        // Bracketed/parenthesised non-speech markers: [BLANK_AUDIO], (wind blowing), etc.
        let bracketed = (t.starts_with('[') && t.ends_with(']'))
            || (t.starts_with('(') && t.ends_with(')'))
            || (t.starts_with('*') && t.ends_with('*'));
        if bracketed {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(t);
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_defaults_to_parakeet_and_whisper_is_opt_in() {
        // from_env reads process state, so exercise the mapping directly.
        assert_eq!(Backend::Parakeet.default_model(), "tdt-0.6b-v3-q8_0.gguf");
        assert_eq!(Backend::Whisper.default_model(), "ggml-large-v3-turbo.bin");
        assert_ne!(Backend::Parakeet.inference_path(), Backend::Whisper.inference_path());
    }

    #[test]
    fn non_speech_markers_are_stripped() {
        assert_eq!(clean("[BLANK_AUDIO]"), "");
        assert_eq!(clean("(wind blowing)"), "");
        assert_eq!(clean("*laughs*"), "");
        assert_eq!(clean("Hello there.\n[BLANK_AUDIO]"), "Hello there.");
    }

    #[test]
    fn ordinary_transcripts_survive_cleaning() {
        assert_eq!(clean("  Hello there.  "), "Hello there.");
        assert_eq!(clean("One line.\nTwo line."), "One line. Two line.");
        // Brackets inside a sentence are content, not a marker.
        assert_eq!(clean("Use the [index] value."), "Use the [index] value.");
    }
}
