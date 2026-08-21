//! Speech recognition via a resident `whisper-server` sidecar.
//!
//! The model is deliberately loaded once and left in VRAM. A cold load of
//! large-v3-turbo is ~840ms; paying that per utterance would dominate the
//! latency budget and make dictation feel sluggish. With the model warm on an
//! RTX 5090, encoding a 30-second window costs ~52ms.
//!
//! The sidecar is supervised: it is assigned to a Windows job object so it can
//! never outlive us and strand a process holding 1.6 GB of VRAM.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::sidecar;

/// Loopback port for the sidecar. Chosen high to avoid common collisions.
const PORT: u16 = 8178;
/// How long to wait for the model to load before declaring the sidecar dead.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone)]
pub struct AsrConfig {
    /// Path to `whisper-server.exe`.
    pub server_bin: PathBuf,
    /// Path to the ggml model file.
    pub model: PathBuf,
    /// Language hint, or "auto" to detect.
    pub language: String,
    /// Use the GPU backend. Falls back to CPU when false.
    pub gpu: bool,
    pub threads: usize,
}

pub struct Asr {
    child: Child,
    client: reqwest::blocking::Client,
    endpoint: String,
}

impl Asr {
    /// Launch the sidecar and block until it answers, so the first dictation
    /// is as fast as every later one.
    pub fn start(config: AsrConfig) -> Result<Self> {
        if !config.server_bin.exists() {
            return Err(anyhow!("whisper-server not found at {}", config.server_bin.display()));
        }
        if !config.model.exists() {
            return Err(anyhow!("model not found at {}", config.model.display()));
        }

        let mut cmd = Command::new(&config.server_bin);
        cmd.arg("-m")
            .arg(&config.model)
            .arg("--host")
            .arg("127.0.0.1")
            .arg("--port")
            .arg(PORT.to_string())
            .arg("-t")
            .arg(config.threads.to_string())
            .arg("-l")
            .arg(&config.language)
            // Dictation is a single short utterance; segment timings are noise.
            .arg("--no-timestamps")
            // Drop [BLANK_AUDIO] / (wind blowing) style tokens at the decoder
            // rather than string-scrubbing them out afterwards.
            .arg("--suppress-nst")
            // Temperature fallback re-decodes on low confidence. That is the
            // right trade for transcribing a podcast and the wrong one for a
            // keystroke-latency path, where a rare imperfect word beats a
            // stall the user can feel.
            .arg("--no-fallback")
            // The DLLs sit beside the exe, so run from there.
            .current_dir(config.server_bin.parent().unwrap_or(Path::new(".")))
            .stdout(Stdio::null())
            .stderr(Stdio::piped());

        if !config.gpu {
            cmd.arg("--no-gpu");
        }

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(sidecar::CREATE_NO_WINDOW);
        }

        tracing::info!(model = %config.model.display(), gpu = config.gpu, "starting ASR sidecar");
        let mut child = cmd.spawn().context("failed to spawn whisper-server")?;

        #[cfg(windows)]
        sidecar::tie_to_parent(&child)?;

        sidecar::await_ready("whisper-server", PORT, "/", STARTUP_TIMEOUT, &mut child)?;

        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(120))
            .build()?;
        let endpoint = format!("http://127.0.0.1:{PORT}/inference");

        let asr = Self { child, client, endpoint };
        asr.warm_up();
        Ok(asr)
    }

    /// Push one throwaway utterance through the model at startup.
    ///
    /// The shipped CUDA binaries carry no `sm_120` cubins, so on RTX 50-series
    /// the driver JIT-compiles the sm_90 PTX on first use. That is a real cost
    /// the user would otherwise pay on their very first dictation - measured at
    /// ~220ms warm-disk-cache, and over a second the first time ever on a
    /// machine. Spending it here instead means every dictation is the fast one.
    ///
    /// Best-effort: a failure here is not fatal, it just means run one is slow.
    fn warm_up(&self) {
        // Low-amplitude noise rather than silence: whisper short-circuits on a
        // silent buffer, which would leave the decode kernels un-compiled.
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
        let form = reqwest::blocking::multipart::Form::new()
            .part("file", part)
            .text("response_format", "json")
            .text("temperature", "0.0");

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

/// Strip the artifacts whisper emits for silence and non-speech audio. These
/// are hallucinations of the training data, not things the user said, and
/// pasting "[BLANK_AUDIO]" into their editor would be worse than pasting
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
