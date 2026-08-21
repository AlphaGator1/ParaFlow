//! Audible session cues.
//!
//! The microphone is opened on demand, which means there is a moment where the
//! user has pressed the key but the device is not live yet. A short rising
//! chime marks the instant it becomes safe to speak; a falling one marks the
//! session closing and the capture light going out.
//!
//! These are the honest signal that the microphone is on. They are quiet, but
//! deliberately not silent.

use once_cell::sync::OnceCell;
use windows::core::PCWSTR;
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};

/// Chime sample rate. 44.1 kHz is accepted by every Windows sound mapper.
const RATE: u32 = 44_100;
/// Length of each tone in the pair.
const TONE_MS: u32 = 70;
/// Kept low - this fires every time a dictation burst starts.
const AMPLITUDE: f32 = 0.18;

/// Total time the readiness chime occupies, so callers can avoid recording
/// over it.
pub const READY_CHIME_MS: u64 = (TONE_MS * 2) as u64;

static READY: OnceCell<&'static [u8]> = OnceCell::new();
static CLOSED: OnceCell<&'static [u8]> = OnceCell::new();

/// Rising pair - the microphone is live, start talking.
pub fn ready() {
    if !crate::settings::current().chimes {
        return;
    }
    play(READY.get_or_init(|| chime(&[660.0, 990.0])));
}

/// Falling pair - the session ended and the microphone is released.
///
/// Suppressible, but worth thinking twice about turning off: this is the
/// audible half of the promise that the microphone is not always listening.
pub fn closed() {
    if !crate::settings::current().chimes {
        return;
    }
    play(CLOSED.get_or_init(|| chime(&[880.0, 587.0])));
}

/// Render a sequence of tones to an in-memory WAV.
///
/// Leaked deliberately: `SND_ASYNC` returns immediately and Windows keeps
/// reading the buffer while it plays, so it has to outlive this call. There
/// are exactly two of these and they are a few KB each, so a one-time leak is
/// simpler and safer than reference counting a buffer the OS holds a raw
/// pointer to.
fn chime(freqs: &[f32]) -> &'static [u8] {
    let per_tone = (RATE * TONE_MS / 1000) as usize;
    let mut samples = Vec::with_capacity(per_tone * freqs.len());

    for &f in freqs {
        for i in 0..per_tone {
            let t = i as f32 / RATE as f32;
            // Raised-cosine envelope. A bare sine starting at full amplitude
            // clicks audibly at both ends.
            let progress = i as f32 / per_tone as f32;
            let envelope = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * progress).cos();
            samples.push((t * f * 2.0 * std::f32::consts::PI).sin() * envelope * AMPLITUDE);
        }
    }

    match crate::audio::encode_wav(&samples, RATE) {
        Ok(wav) => Box::leak(wav.into_boxed_slice()),
        Err(e) => {
            tracing::warn!(error = %e, "failed to render chime; session cues will be silent");
            Box::leak(Vec::new().into_boxed_slice())
        }
    }
}

fn play(wav: &'static [u8]) {
    if wav.is_empty() {
        return;
    }
    unsafe {
        // SND_MEMORY reinterprets the "filename" pointer as a WAV image.
        let ok = PlaySoundW(
            PCWSTR(wav.as_ptr() as *const u16),
            None,
            SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
        );
        if !ok.as_bool() {
            tracing::debug!("PlaySound rejected the chime");
        }
    }
}
