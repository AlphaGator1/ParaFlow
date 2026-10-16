//! The dictation state machine.
//!
//! Two nested lifetimes run here. A *session* spans a burst of dictation: it
//! opens the microphone on the first press and closes it once the burst goes
//! quiet, so the capture light is only lit while ParaFlow is actually in use.
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

/// How long a hands-free recording may run before it stops itself.
///
/// Bounded by the capture ring rather than by taste. [`audio::RING_SECONDS`]
/// of audio is kept, and `Ring::slice` clamps to what is still resident rather
/// than erroring - so a latch that outran the ring would silently lose its own
/// beginning and nobody would be told. Stopping short of that, with the
/// countdown visible on the overlay for the last stretch, is the honest
/// version of the same limit.
///
/// It doubles as a safety net: a latched microphone left running because
/// somebody walked away shuts itself off.
const MAX_LATCHED: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum Stage {
    /// Models are loading and the keyboard hook is not installed yet, so the
    /// push-to-talk key genuinely does nothing. Distinct from `Idle` because
    /// the overlay used to claim "Ready" through this whole window and swallow
    /// the user's first press without a sound.
    Starting,
    Idle,
    /// The microphone is being opened - first press of a burst.
    Waking,
    Listening,
    /// Recording hands-free. The key is not held; only a press stops it.
    Latched {
        elapsed_s: u64,
        remaining_s: u64,
    },
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

/// The utterance in progress.
///
/// `latched` lives here rather than beside it so the two cannot disagree:
/// there is no state in which the pipeline believes it is latched but has no
/// recording, which is what the idle sweep relies on to leave the microphone
/// alone.
struct Active {
    /// Ring offset recording started from.
    start: u64,
    /// When capture began - the tap filter and the latch ceiling measure from
    /// here. The latency figure does not: it starts at release.
    began: Instant,
    /// True when recording is latched on rather than key-held.
    latched: bool,
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
}

impl Pipeline {
    pub fn new(
        mic: MicSession,
        asr: Asr,
        polish: Option<Polish>,
        app: AppHandle,
    ) -> Self {
        Self { mic, asr, polish, app }
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
        let mut active: Option<Active> = None;
        let mut last_activity = Instant::now();
        let listening = Arc::new(AtomicBool::new(false));

        self.emit(Stage::Idle);
        self.emit_mic(Duration::ZERO);
        let settings = crate::settings::current();
        tracing::info!(
            asr = ?self.asr.backend(),
            cleanup = self.polish.is_some(),
            idle_timeout_s = settings.effective_idle().as_secs(),
            hands_free = settings.effective_hands_free(),
            "pipeline ready; microphone stays closed until first use"
        );

        loop {
            match rx.recv_timeout(TICK) {
                Ok(PttEvent::Down) => {
                    if let Some(a) = active.take() {
                        // A press while latched is the stop signal. Anything
                        // else is a key repeat inside a held utterance.
                        if a.latched {
                            self.finish(a, &listening, &mut last_activity);
                        } else {
                            active = Some(a);
                        }
                        continue;
                    }
                    active = self.start_utterance(false, &listening);
                }

                Ok(PttEvent::Up) => {
                    // Latched recording ignores the key entirely - that is the
                    // whole point of it.
                    if active.as_ref().is_some_and(|a| a.latched) {
                        continue;
                    }
                    let Some(a) = active.take() else { continue };
                    self.finish(a, &listening, &mut last_activity);
                }

                Ok(PttEvent::ToggleLatch) => {
                    if let Some(a) = active.take() {
                        self.finish(a, &listening, &mut last_activity);
                        continue;
                    }
                    active = self.start_utterance(true, &listening);
                }

                Err(RecvTimeoutError::Timeout) => {
                    // Mid-utterance the microphone is in use, so the idle sweep
                    // has nothing to do. A latched recording still has its own
                    // ceiling to watch.
                    if let Some(a) = active.take() {
                        if !a.latched {
                            active = Some(a);
                            continue;
                        }
                        let elapsed = a.began.elapsed();
                        if elapsed >= MAX_LATCHED {
                            tracing::info!(
                                secs = elapsed.as_secs(),
                                "hands-free hit its ceiling; stopping and transcribing"
                            );
                            self.finish(a, &listening, &mut last_activity);
                        } else {
                            self.emit(latched_stage(elapsed));
                            active = Some(a);
                        }
                        continue;
                    }

                    if self.mic.is_open() {
                        // Read per sweep rather than caching, so changing the
                        // timeout in settings takes effect immediately.
                        let limit = crate::settings::current().effective_idle();
                        let idle_for = last_activity.elapsed();
                        if idle_for >= limit {
                            self.end_session();
                        } else {
                            self.emit_mic(limit - idle_for);
                        }
                    }
                }

                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    }

    /// Open the microphone if needed and start recording. Returns `None` when
    /// the microphone could not be opened, having already reported it.
    fn start_utterance(&mut self, latched: bool, listening: &Arc<AtomicBool>) -> Option<Active> {
        match self.begin_utterance() {
            Ok(start) => {
                listening.store(true, Ordering::Relaxed);
                if latched {
                    crate::hotkey::set_latched(true);
                    tracing::info!(ceiling_s = MAX_LATCHED.as_secs(), "hands-free engaged");
                    self.emit(latched_stage(Duration::ZERO));
                } else {
                    self.emit(Stage::Listening);
                }
                self.spawn_level_meter(start, listening.clone());
                Some(Active { start, began: Instant::now(), latched })
            }
            Err(e) => {
                tracing::error!(error = %e, latched, "could not start listening");
                self.emit(Stage::Error { message: e.to_string() });
                None
            }
        }
    }

    /// End the utterance: transcribe, clean up and paste, or discard a tap.
    fn finish(&mut self, a: Active, listening: &Arc<AtomicBool>, last_activity: &mut Instant) {
        // The overlay's "Pasted in N ms" is release-to-paste, the wait the
        // user actually feels. Timing it from `began` would count the whole
        // time they spent speaking.
        let released = Instant::now();
        listening.store(false, Ordering::Relaxed);
        if a.latched {
            // Cleared before the slow work below, not after. While it is set
            // the hook turns every press into ToggleLatch. A press during
            // transcription waits in the channel, and by the time it is read
            // nothing is recording - so ToggleLatch would start a hands-free
            // recording, and the release that should have ended an ordinary
            // dictation would be ignored, leaving the microphone on.
            crate::hotkey::set_latched(false);
        }
        *last_activity = Instant::now();

        let held = a.began.elapsed().as_millis();
        tracing::info!(held, latched = a.latched, "utterance ended");
        if held < MIN_UTTERANCE_MS {
            tracing::info!(held, min = MIN_UTTERANCE_MS, "ignoring tap: too short");
            self.emit(Stage::Idle);
            return;
        }

        match self.handle_utterance(a.start, released) {
            Ok(Some(stage)) => self.emit(stage),
            Ok(None) => self.emit(Stage::Idle),
            Err(e) => {
                tracing::error!(error = %e, "dictation failed");
                self.emit(Stage::Error { message: e.to_string() });
            }
        }
        // Transcription can take a moment; the idle clock should start from
        // when we finished, not when the key came up.
        *last_activity = Instant::now();
        self.emit_mic(crate::settings::current().effective_idle());
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
        self.emit_mic(crate::settings::current().effective_idle());

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
    fn handle_utterance(&self, start: u64, released: Instant) -> anyhow::Result<Option<Stage>> {
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

        Ok(Some(Stage::Done { text, latency_ms: released.elapsed().as_millis() }))
    }

    /// Feed the overlay a peak level while recording, so the UI can show the
    /// mic is actually hearing something.
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

/// The latched stage as the overlay should see it, with the ceiling counted
/// down rather than hidden. A hands-free recording that stops without warning
/// looks like a crash.
fn latched_stage(elapsed: Duration) -> Stage {
    Stage::Latched {
        elapsed_s: elapsed.as_secs(),
        remaining_s: MAX_LATCHED.saturating_sub(elapsed).as_secs(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one invariant the latch cannot survive losing.
    ///
    /// `Ring::slice` clamps to whatever is still resident instead of failing,
    /// so if the ceiling ever crept past the ring the only symptom would be
    /// transcripts quietly missing their first sentence.
    #[test]
    fn the_latch_ceiling_stays_inside_the_capture_ring() {
        let ceiling = MAX_LATCHED.as_secs();
        let ring = audio::RING_SECONDS as u64;
        assert!(
            ceiling < ring,
            "latch ceiling {ceiling}s must fit in the {ring}s ring"
        );
        // Room for the pre-roll and for the gap between hitting the ceiling on
        // a 500ms tick and actually reading the samples out.
        assert!(
            ring - ceiling >= 10,
            "only {}s of headroom between the ceiling and the ring",
            ring - ceiling
        );
    }

    #[test]
    fn the_countdown_reaches_zero_exactly_at_the_ceiling() {
        let Stage::Latched { elapsed_s, remaining_s } = latched_stage(Duration::ZERO) else {
            panic!("expected a latched stage");
        };
        assert_eq!(elapsed_s, 0);
        assert_eq!(remaining_s, MAX_LATCHED.as_secs());

        let Stage::Latched { remaining_s, .. } = latched_stage(MAX_LATCHED) else {
            panic!("expected a latched stage");
        };
        assert_eq!(remaining_s, 0);
    }

    #[test]
    fn the_countdown_does_not_underflow_past_the_ceiling() {
        // The sweep runs on a 500ms tick, so `elapsed` is always a little past
        // the ceiling by the time it is noticed. Duration subtraction panics
        // on underflow, which would take the pipeline thread down with it.
        let Stage::Latched { remaining_s, .. } =
            latched_stage(MAX_LATCHED + Duration::from_secs(30))
        else {
            panic!("expected a latched stage");
        };
        assert_eq!(remaining_s, 0);
    }
}
