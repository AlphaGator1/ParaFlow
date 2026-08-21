//! Microphone capture, opened and closed on demand.
//!
//! An always-open microphone is the low-latency design, but it means the
//! capture light is lit all day for an app used in short bursts. So the device
//! is opened when a dictation session starts and closed again once the session
//! goes idle - the OS microphone indicator becomes an honest signal of whether
//! ParaFlow is listening.
//!
//! Within a session the stream stays open, so only the *first* press of a burst
//! pays the device-open cost. Every press after that keeps the original
//! always-warm behaviour, including the pre-roll that catches a syllable
//! spoken slightly ahead of the key.

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, SampleFormat, StreamConfig};
use parking_lot::Mutex;
use rubato::{FftFixedIn, Resampler};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

/// Whisper is trained on 16 kHz mono; anything else has to be converted.
pub const TARGET_RATE: u32 = 16_000;
/// Audio kept behind the press instant, to catch a syllable that beat the key.
const PREROLL_MS: u64 = 300;
/// Ring capacity, in seconds of audio.
///
/// A press-and-hold utterance is seconds long, but a hands-free latch runs
/// until the user stops it, and `Ring::slice` clamps silently rather than
/// erroring - so a latch that outran the ring would quietly lose its own
/// beginning. This is deliberately larger than `pipeline::MAX_LATCHED`, and
/// `pipeline` has a test asserting the headroom.
pub const RING_SECONDS: usize = 150;

struct Ring {
    buf: VecDeque<f32>,
    /// Monotonic count of samples ever written. Survives the ring wrapping,
    /// so marks stay valid even as old audio is evicted.
    written: u64,
    capacity: usize,
}

impl Ring {
    fn push(&mut self, sample: f32) {
        if self.buf.len() == self.capacity {
            self.buf.pop_front();
        }
        self.buf.push_back(sample);
        self.written += 1;
    }

    /// Copy out samples in the absolute range `[from, to)`, clamped to whatever
    /// is still resident in the ring.
    fn slice(&self, from: u64, to: u64) -> Vec<f32> {
        let oldest = self.written - self.buf.len() as u64;
        let from = from.max(oldest);
        let to = to.min(self.written);
        if to <= from {
            return Vec::new();
        }
        let start = (from - oldest) as usize;
        let end = (to - oldest) as usize;
        self.buf.iter().skip(start).take(end - start).copied().collect()
    }
}

/// An open capture device plus the ring it feeds.
struct Active {
    /// Held only to keep the stream alive; dropping it closes the mic and
    /// turns off the OS capture indicator.
    _stream: cpal::Stream,
    ring: Arc<Mutex<Ring>>,
    rate: u32,
}

/// A microphone that is only open while it is being used.
///
/// Not `Send`: `cpal::Stream` is thread-affine on Windows, so this must live on
/// the thread that opens it. Use [`MicSession::probe`] to hand a level meter to
/// another thread.
pub struct MicSession {
    active: Option<Active>,
    device_name: String,
}

impl MicSession {
    /// Prepare a session without touching the microphone.
    pub fn new() -> Self {
        Self { active: None, device_name: String::from("<not yet opened>") }
    }

    pub fn is_open(&self) -> bool {
        self.active.is_some()
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    /// Open the default input device. No-op if already open.
    ///
    /// Returns how long the device took to come up, which is the latency the
    /// user perceives on the first press of a burst.
    pub fn open(&mut self) -> Result<std::time::Duration> {
        if self.active.is_some() {
            return Ok(std::time::Duration::ZERO);
        }
        let began = Instant::now();

        let host = cpal::default_host();
        let device: Device = host
            .default_input_device()
            .ok_or_else(|| anyhow!("no default input device - is a microphone connected?"))?;
        self.device_name = device.name().unwrap_or_else(|_| "<unknown>".into());

        let supported = device
            .default_input_config()
            .context("failed to read default input config")?;
        let rate = supported.sample_rate().0;
        let channels = supported.channels() as usize;
        let sample_format = supported.sample_format();
        let config: StreamConfig = supported.into();

        let ring = Arc::new(Mutex::new(Ring {
            buf: VecDeque::with_capacity(rate as usize * RING_SECONDS),
            written: 0,
            capacity: rate as usize * RING_SECONDS,
        }));

        let sink = ring.clone();
        // Downmix to mono on the way in - whisper wants one channel and this
        // keeps the ring a third the size on a typical stereo device.
        let push_frames = move |data: &[f32]| {
            let mut r = sink.lock();
            for frame in data.chunks(channels) {
                let mono = frame.iter().sum::<f32>() / channels as f32;
                r.push(mono);
            }
        };

        let err_fn = |e| tracing::error!(error = %e, "audio stream error");

        let stream = match sample_format {
            SampleFormat::F32 => {
                device.build_input_stream(&config, move |d: &[f32], _| push_frames(d), err_fn, None)
            }
            SampleFormat::I16 => device.build_input_stream(
                &config,
                move |d: &[i16], _| {
                    let f: Vec<f32> = d.iter().map(|s| *s as f32 / i16::MAX as f32).collect();
                    push_frames(&f)
                },
                err_fn,
                None,
            ),
            SampleFormat::U16 => device.build_input_stream(
                &config,
                move |d: &[u16], _| {
                    let f: Vec<f32> =
                        d.iter().map(|s| (*s as f32 / u16::MAX as f32) * 2.0 - 1.0).collect();
                    push_frames(&f)
                },
                err_fn,
                None,
            ),
            other => return Err(anyhow!("unsupported sample format: {other:?}")),
        }
        .context("failed to build input stream")?;

        stream.play().context("failed to start capture stream")?;
        self.active = Some(Active { _stream: stream, ring, rate });

        let took = began.elapsed();
        tracing::info!(
            device = %self.device_name, rate, channels, ?sample_format,
            ms = took.as_millis(), "microphone opened"
        );
        Ok(took)
    }

    /// Release the microphone. The OS capture indicator goes out.
    pub fn close(&mut self) {
        if self.active.take().is_some() {
            tracing::info!(device = %self.device_name, "microphone closed");
        }
    }

    /// Mark the start of an utterance, reaching back by the pre-roll window.
    ///
    /// `None` if the microphone is not open.
    pub fn mark(&self) -> Option<u64> {
        let a = self.active.as_ref()?;
        let r = a.ring.lock();
        let preroll = a.rate as u64 * PREROLL_MS / 1000;
        Some(r.written.saturating_sub(preroll))
    }

    /// Mark the current instant with no pre-roll.
    ///
    /// Used at session start: the audio behind us is the readiness chime, not
    /// the user, and feeding a beep to the recogniser helps nobody.
    pub fn mark_now(&self) -> Option<u64> {
        let a = self.active.as_ref()?;
        Some(a.ring.lock().written)
    }

    /// A cloneable, thread-safe view of the level meter.
    pub fn probe(&self) -> Option<LevelProbe> {
        let a = self.active.as_ref()?;
        Some(LevelProbe { ring: a.ring.clone(), device_rate: a.rate })
    }

    /// Take everything from `mark` to now, resampled to 16 kHz mono.
    pub fn take_since(&self, mark: u64) -> Result<Vec<f32>> {
        let Some(a) = self.active.as_ref() else {
            return Ok(Vec::new());
        };
        let raw = {
            let r = a.ring.lock();
            r.slice(mark, r.written)
        };
        if raw.is_empty() {
            return Ok(Vec::new());
        }
        resample(&raw, a.rate, TARGET_RATE)
    }
}

/// Read-only handle onto the capture ring, safe to move between threads.
#[derive(Clone)]
pub struct LevelProbe {
    ring: Arc<Mutex<Ring>>,
    device_rate: u32,
}

impl LevelProbe {
    /// Peak over roughly the last 50ms, never reaching back before `mark`.
    pub fn level_since(&self, mark: u64) -> f32 {
        let r = self.ring.lock();
        let window = self.device_rate as u64 / 20;
        let from = r.written.saturating_sub(window).max(mark);
        peak(&r.slice(from, r.written))
    }
}

/// Rational resample to 16 kHz. Devices almost always run at 44.1k or 48k, and
/// naive decimation would alias speech harmonics down into the voice band and
/// cost real word-error-rate, so this goes through a proper FFT resampler.
fn resample(input: &[f32], from: u32, to: u32) -> Result<Vec<f32>> {
    if from == to {
        return Ok(input.to_vec());
    }
    let mut rs = FftFixedIn::<f32>::new(from as usize, to as usize, 1024, 2, 1)
        .context("failed to build resampler")?;

    // How many output samples the conversion owes us.
    let expected = input.len() * to as usize / from as usize;
    // The FFT resampler emits a fixed group delay of leading zeros before real
    // output begins. Those have to be skipped, and enough extra input flushed
    // through to push the genuine tail out - otherwise the last few
    // milliseconds of the utterance are silently dropped, which clips the final
    // consonant of the last word.
    let delay = rs.output_delay();

    let mut out: Vec<f32> = Vec::with_capacity(expected + delay + 1024);
    let mut pos = 0usize;

    // Ask the resampler how much it wants rather than assuming a fixed chunk;
    // FftFixedIn varies its appetite between calls.
    while out.len() < expected + delay {
        let need = rs.input_frames_next();
        let end = (pos + need).min(input.len());
        let mut block = Vec::with_capacity(need);
        block.extend_from_slice(&input[pos.min(input.len())..end]);
        // Past the end of the real audio we feed silence, purely to flush.
        block.resize(need, 0.0);
        pos = end;

        let done = rs.process(&[block], None).context("resample failed")?;
        out.extend_from_slice(&done[0]);
    }

    out.drain(..delay.min(out.len()));
    out.truncate(expected);
    Ok(out)
}

/// Encode 16 kHz mono f32 as a 16-bit PCM WAV in memory for the ASR sidecar.
pub fn to_wav(samples: &[f32]) -> Result<Vec<u8>> {
    encode_wav(samples, TARGET_RATE)
}

/// Encode arbitrary mono f32 samples as a 16-bit PCM WAV.
pub fn encode_wav(samples: &[f32], rate: u32) -> Result<Vec<u8>> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: rate,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut cursor = std::io::Cursor::new(Vec::<u8>::new());
    {
        let mut w = hound::WavWriter::new(&mut cursor, spec)?;
        for s in samples {
            w.write_sample((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
        }
        w.finalize()?;
    }
    Ok(cursor.into_inner())
}

/// Peak amplitude, for the overlay's level meter.
pub fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0f32, |m, s| m.max(s.abs()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(capacity: usize) -> Ring {
        Ring { buf: VecDeque::with_capacity(capacity), written: 0, capacity }
    }

    #[test]
    fn slice_returns_the_requested_absolute_range() {
        let mut r = ring(10);
        for i in 0..8 {
            r.push(i as f32);
        }
        assert_eq!(r.slice(2, 5), vec![2.0, 3.0, 4.0]);
    }

    #[test]
    fn marks_stay_valid_after_the_ring_wraps() {
        let mut r = ring(4);
        for i in 0..10 {
            r.push(i as f32);
        }
        // Only the last 4 samples (6..10) are still resident.
        assert_eq!(r.written, 10);
        assert_eq!(r.slice(6, 10), vec![6.0, 7.0, 8.0, 9.0]);
        // A mark older than the ring clamps rather than panicking.
        assert_eq!(r.slice(0, 10), vec![6.0, 7.0, 8.0, 9.0]);
    }

    #[test]
    fn empty_and_inverted_ranges_are_empty() {
        let mut r = ring(8);
        for i in 0..4 {
            r.push(i as f32);
        }
        assert!(r.slice(3, 3).is_empty());
        assert!(r.slice(4, 2).is_empty());
        assert!(r.slice(99, 120).is_empty());
    }

    fn tone(samples: usize, hz: f32, rate: f32) -> Vec<f32> {
        (0..samples).map(|i| (i as f32 * 2.0 * std::f32::consts::PI * hz / rate).sin()).collect()
    }

    #[test]
    fn resampling_48k_to_16k_keeps_every_sample_it_owes() {
        let out = resample(&tone(4800, 440.0, 48_000.0), 48_000, 16_000).unwrap();
        // Exact, not approximate: dropping frames here silently clips the end
        // of the utterance.
        assert_eq!(out.len(), 1600);
        // A 440 Hz tone is well under the 8 kHz Nyquist limit, so it must
        // survive with its amplitude broadly intact rather than aliasing away.
        assert!(peak(&out) > 0.5, "signal collapsed: peak {}", peak(&out));
    }

    #[test]
    fn resampling_44k1_to_16k_keeps_every_sample_it_owes() {
        // The awkward ratio: 147:160 rather than a clean 3:1.
        let out = resample(&tone(44_100, 440.0, 44_100.0), 44_100, 16_000).unwrap();
        assert_eq!(out.len(), 16_000);
        assert!(peak(&out) > 0.5, "signal collapsed: peak {}", peak(&out));
    }

    #[test]
    fn the_tail_of_the_signal_survives_resampling() {
        // Silence, then a burst at the very end. If the flush is wrong, the
        // burst is the part that gets dropped.
        let mut input = vec![0.0f32; 4000];
        input.extend(tone(800, 440.0, 48_000.0));
        let out = resample(&input, 48_000, 16_000).unwrap();

        let tail_peak = peak(&out[out.len() * 4 / 5..]);
        assert!(tail_peak > 0.5, "end of utterance was lost: tail peak {tail_peak}");
    }

    /// Not an assertion so much as a measurement, printed under
    /// `cargo test --release -- --nocapture`. The batch resample at key-release
    /// is the one stage whose cost scales with utterance length, so it is worth
    /// knowing what it actually costs before optimising it away.
    #[test]
    fn report_resample_cost_for_a_long_utterance() {
        // 10.69 s at 48 kHz - the utterance measured end-to-end in the README.
        let input = tone(513_120, 440.0, 48_000.0);

        let began = std::time::Instant::now();
        let out = resample(&input, 48_000, 16_000).unwrap();
        let took = began.elapsed();

        let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
        println!(
            "resample {:.2}s audio [{profile}]: {:?} ({} -> {} samples)",
            input.len() as f32 / 48_000.0,
            took,
            input.len(),
            out.len()
        );
    }

    #[test]
    fn a_short_utterance_still_resamples() {
        // Shorter than one internal chunk.
        let out = resample(&tone(300, 440.0, 48_000.0), 48_000, 16_000).unwrap();
        assert_eq!(out.len(), 100);
    }

    #[test]
    fn matching_rates_pass_through_untouched() {
        let input = vec![0.1, -0.2, 0.3];
        assert_eq!(resample(&input, 16_000, 16_000).unwrap(), input);
    }
}
