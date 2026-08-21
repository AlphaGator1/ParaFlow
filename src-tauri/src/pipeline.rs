//! The dictation state machine.
//!
//! Two nested lifetimes run here. A *session* spans a burst of dictation: it
//! opens the microphone on the first press and closes it once the burst goes
//! quiet, so the capture light is only lit while FlowState is actually in use.
//! An *utterance* is one press-and-hold inside that session.
//!
//! Everything runs off the UI thread. The hook thread only pushes events into a
//! channel, so a slow transcription can never wedge the keyboard hook (Windows
//! silently uninstalls hooks that block) or freeze the tray.

use crate::asr::Asr;
use crate::audio::{self, MicSession};
use crate::hotkey::PttEvent;
use crate::inject;
use crate::polish::{Polish, Style};
use crate::sound;

use crossbeam_channel::{Receiver, RecvTimeoutError};
use serde::Serialize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

/// Utterances shorter than this are almost always a mis-press, not speech.
const MIN_UTTERANCE_MS: u128 = 250;

/// Peak amplitude below which a buffer is treated as silence. Deliberately low:
/// laptop and webcam microphones are quiet, and discarding real speech is a far
/// worse failure than occasionally transcribing room tone.
const NOISE_FLOOR: f32 = 0.003;

/// How often the loop wakes to check whether the session has gone idle.
const TICK: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Stage {
    Idle,
    /// The microphone is being opened - first press of a burst.
    Waking,
    Listening,
    Transcribing,
    Polishing,
    Done {
        text: String,
        latency_ms: u128,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct LevelEvent {
    pub peak: f32,
}

/// Whether the microphone is currently open, and how long it has left.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MicEvent {
    pub open: bool,
    pub device: String,
    pub closes_in_s: u64,
}

pub struct Pipeline {
    mic: MicSession,
    asr: Asr,
    polish: Option<Polish>,
    app: AppHandle,
    /// Idle time after which the microphone is released.
    idle_timeout: Duration,
}

impl Pipeline {
    pub fn new(
        mic: MicSession,
        asr: Asr,
        polish: Option<Polish>,
        app: AppHandle,
        idle_timeout: Duration,
    ) -> Self {
        Self { mic, asr, polish, app, idle_timeout }
    }

    fn emit(&self, stage: Stage) {
        if let Err(e) = self.app.emit("flow://stage", &stage) {
            tracing::warn!(error = %e, "failed to emit stage event");
        }
    }

    fn emit_mic(&self, closes_in: Duration) {
        let _ = self.app.emit(
            "flow://mic",
            MicEvent {
                open: self.mic.is_open(),
                device: self.mic.device_name().to_string(),
                closes_in_s: closes_in.as_secs(),
            },
        );
    }

    /// Consume push-to-talk events until the channel closes.
    pub fn run(mut self, rx: Receiver<PttEvent>) {
        let mut mark: Option<u64> = None;
        let mut began = Instant::now();
        let mut last_activity = Instant::now();
        let listening = Arc::new(AtomicBool::new(false));

        self.emit(Stage::Idle);
        self.emit_mic(Duration::ZERO);
        tracing::info!(
            asr = ?self.asr.backend(),
            cleanup = self.polish.is_some(),
            idle_timeout_s = self.idle_timeout.as_secs(),
            "pipeline ready; microphone stays closed until first use"
        );

        loop {
            match rx.recv_timeout(TICK) {
                Ok(PttEvent::Down) => {
                    if mark.is_some() {
                        continue; // already recording; ignore key repeat
                    }
                    match self.begin_utterance() {
                        Ok(start) => {
                            mark = Some(start);
                            began = Instant::now();
                            listening.store(true, Ordering::Relaxed);
                            self.emit(Stage::Listening);
                            self.spawn_level_meter(start, listening.clone());
                        }
                        Err(e) => {
                            tracing::error!(error = %e, "could not start listening");
                            self.emit(Stage::Error { message: e.to_string() });
                        }
                    }
                }

                Ok(PttEvent::Up) => {
                    listening.store(false, Ordering::Relaxed);
                    let Some(start) = mark.take() else { continue };
                    last_activity = Instant::now();

                    let held = began.elapsed().as_millis();
                    tracing::info!(held, "push-to-talk up");
                    if held < MIN_UTTERANCE_MS {
                        tracing::info!(held, min = MIN_UTTERANCE_MS, "ignoring tap: too short");
                        self.emit(Stage::Idle);
                        continue;
                    }

                    match self.handle_utterance(start, began) {
                        Ok(Some(stage)) => self.emit(stage),
                        Ok(None) => self.emit(Stage::Idle),
                        Err(e) => {
                            tracing::error!(error = %e, "dictation failed");
                            self.emit(Stage::Error { message: e.to_string() });
                        }
                    }
                    // Transcription can take a moment; the idle clock should
                    // start from when we finished, not when the key came up.
                    last_activity = Instant::now();
                    self.emit_mic(self.idle_timeout);
                }

                Ok(PttEvent::ToggleLatch) => {
                    // Hands-free mode is a later milestone.
                }

                Err(RecvTimeoutError::Timeout) => {
                    // Idle sweep. Never close mid-utterance.
                    if self.mic.is_open() && mark.is_none() {
                        let idle_for = last_activity.elapsed();
                        if idle_for >= self.idle_timeout {
                            self.end_session();
                        } else {
                            self.emit_mic(self.idle_timeout - idle_for);
                        }
                    }
                }

                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    /// Open the microphone if this is the first press of a burst, and return
    /// the offset recording should start from.
    fn begin_utterance(&mut self) -> anyhow::Result<u64> {
        if self.mic.is_open() {
            // Session already live: instant, and with pre-roll.
            return self
                .mic
                .mark()
                .ok_or_else(|| anyhow::anyhow!("microphone closed unexpectedly"));
        }

        self.emit(Stage::Waking);
        let took = self.mic.open()?;
        self.emit_mic(self.idle_timeout);

        // Tell the user the microphone is live. The chime plays through the
        // speakers and the microphone is already open, so it lands in the ring
        // - marking after it keeps a beep out of the transcript.
        sound::ready();
        std::thread::sleep(Duration::from_millis(sound::READY_CHIME_MS));

        tracing::info!(open_ms = took.as_millis(), "session started");
        self.mic
            .mark_now()
            .ok_or_else(|| anyhow::anyhow!("microphone closed unexpectedly"))
    }

    /// Release the microphone and tell the user it happened.
    fn end_session(&mut self) {
        self.mic.close();
        sound::closed();
        self.emit_mic(Duration::ZERO);
        self.emit(Stage::Idle);
    }

    /// Pull the audio, transcribe it, clean it up, and paste. Returns `None`
    /// when the audio held no speech worth pasting.
    fn handle_utterance(&self, start: u64, began: Instant) -> anyhow::Result<Option<Stage>> {
        self.emit(Stage::Transcribing);

        let samples = self.mic.take_since(start)?;
        let secs = samples.len() as f32 / audio::TARGET_RATE as f32;
        let level = audio::peak(&samples);
        tracing::info!(samples = samples.len(), secs, level, "captured utterance");

        if samples.is_empty() {
            tracing::warn!("no audio captured - is the microphone in use by another app?");
            return Ok(None);
        }
        if level < NOISE_FLOOR {
            tracing::warn!(level, floor = NOISE_FLOOR, "below noise floor, discarding");
            return Ok(None);
        }

        let wav = audio::to_wav(&samples)?;
        let raw = self.asr.transcribe(wav)?;
        if raw.is_empty() {
            tracing::warn!(secs, level, "transcript came back empty");
            return Ok(None);
        }

        // Read the target app *before* cleanup: the user is still focused on
        // it now, and cleanup takes long enough that they could alt-tab away
        // and have the text styled for the wrong window.
        let process = inject::foreground_process();
        let style = Style::for_process(process.as_deref());

        // Cleanup is best-effort by construction: `apply` returns the raw
        // transcript on any failure, so this can only improve the text or
        // leave it alone.
        let text = match &self.polish {
            Some(p) => {
                self.emit(Stage::Polishing);
                p.apply(&raw, style)
            }
            None => raw.clone(),
        };
        if text != raw {
            tracing::debug!(%raw, %text, "cleanup rewrote transcript");
        }
        if text.is_empty() {
            return Ok(None);
        }

        let target = inject::foreground_window_title().unwrap_or_default();
        tracing::info!(
            secs, chars = text.len(), %target,
            app = process.as_deref().unwrap_or("?"), ?style,
            "pasting transcript"
        );
        inject::paste(&text)?;

        Ok(Some(Stage::Done { text, latency_ms: began.elapsed().as_millis() }))
    }

    /// Feed the overlay a peak level while the key is held, so the UI can show
    /// the mic is actually hearing something.
    fn spawn_level_meter(&self, mark: u64, listening: Arc<AtomicBool>) {
        let Some(probe) = self.mic.probe() else { return };
        let app = self.app.clone();
        std::thread::spawn(move || {
            while listening.load(Ordering::Relaxed) {
                let _ = app.emit("flow://level", LevelEvent { peak: probe.level_since(mark) });
                std::thread::sleep(Duration::from_millis(50));
            }
            // Settle the meter back to zero so the overlay does not freeze mid-bar.
            let _ = app.emit("flow://level", LevelEvent { peak: 0.0 });
        });
    }
}
