//! Cross-platform audio I/O for the chatx call path (cpal-backed).
//!
//! Normalises to **s16le (little-endian 16-bit signed)** PCM, which is the
//! canonical layout the rest of the codebase and the mobile shells use.
//!
//! - **Microphone input**: install a sink with [`set_input_sink`], then
//!   [`Audio::start_input`]. Every device frame is converted to s16le and
//!   delivered to your sink with the negotiated sample rate + channel count.
//!
//!   **Important**: the sink is captured at stream-build time. Install it
//!   *before* calling `start_input`.
//!
//! - **Speaker output**: [`Audio::queue_output`] any s16le buffer at any
//!   time, then [`Audio::start_output`]. The OS render callback dequeues
//!   s16le samples and writes f32 values into cpal's frame, zero-filling
//!   (silence) for any shortfall.
//!
//! ## Threads
//!
//! Callbacks run on the OS audio thread (Core Audio on iOS/macOS, AAudio on
//! Android, WASAPI on Windows). Keep them cheap — hop to the app thread for
//! anything non-trivial.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, OnceLock};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// Incoming microphone frame, already packed to s16le.
///
/// - `bytes` — raw s16le PCM (`len == samples_total * 2`)
/// - `rate` — sample rate in Hz as negotiated with the driver
/// - `channels` — 1 or 2
pub type InputCb = Box<dyn Fn(&[u8], u32, u32) + Send + 'static>;

/// Shared input-sink slot. The audio thread reads this on every stream tick;
/// the app thread writes it whenever the UI wants a new callback.
static INPUT_SINK: OnceLock<Mutex<Option<InputCb>>> = OnceLock::new();

/// A pair of cpal streams (input = mic, output = speaker) plus the
/// speaker-side sample queue. Use the singleton `start_input` / `start_output`
/// below to drive it globally, or construct directly for tests.
pub struct Audio {
    input: RefCell<Option<cpal::Stream>>,
    output: RefCell<Option<cpal::Stream>>,
    /// Pending s16le sample buffers to play next, FIFO. Consumed by the
    /// render callback in `start_output`.
    out_queue: Arc<Mutex<VecDeque<u8>>>,
    in_rate: std::cell::Cell<u32>,
    in_ch: std::cell::Cell<u32>,
    out_rate: std::cell::Cell<u32>,
    out_ch: std::cell::Cell<u32>,
}

impl Default for Audio {
    fn default() -> Self {
        Self::new()
    }
}

impl Audio {
    pub fn new() -> Self {
        Audio {
            input: RefCell::new(None),
            output: RefCell::new(None),
            out_queue: Arc::new(Mutex::new(VecDeque::new())),
            in_rate: std::cell::Cell::new(0),
            in_ch: std::cell::Cell::new(0),
            out_rate: std::cell::Cell::new(0),
            out_ch: std::cell::Cell::new(0),
        }
    }

    pub fn in_rate(&self) -> u32 {
        self.in_rate.get()
    }

    /// Access the speaker output queue from any thread. The audio render
    /// callback (running on cpal's audio thread) consumes from this queue, so
    /// producers may be on any thread — just extend the `VecDeque` with
    /// s16le bytes.
    ///
    /// Useful for WebRTC / network sinks that run on their own runtime
    /// threads and cannot borrow `Audio` (which is not `Sync` because of its
    /// `RefCell`-held cpal streams).
    pub fn output_queue(&self) -> &Arc<Mutex<VecDeque<u8>>> {
        &self.out_queue
    }

    /// Append bytes to the speaker queue from any thread, bounded the same
    /// way as [`queue_output`](Self::queue_output). This is the
    /// thread-safe entry point for remote-media sinks.
    pub fn push_output_shared(&self, s16le: Vec<u8>) {
        let mut q = self.out_queue.lock().unwrap();
        q.extend(s16le);
        const CAP: usize = 5 * 48_000 * 2 * 2;
        while q.len() > CAP {
            q.pop_front();
        }
    }
    pub fn in_channels(&self) -> u32 {
        self.in_ch.get()
    }
    pub fn out_rate(&self) -> u32 {
        self.out_rate.get()
    }
    pub fn out_channels(&self) -> u32 {
        self.out_ch.get()
    }

    /// Append an s16le buffer to the speaker queue. Safe to call any time —
    /// even before `start_output`. The buffer is taken by value; the audio
    /// thread consumes it.
    ///
    /// Bounded to ~5s of audio (at 48 kHz stereo) — excess samples are
    /// dropped from the front so a runaway producer cannot wedge playback.
    pub fn queue_output(&self, s16le: Vec<u8>) {
        let mut q = self.out_queue.lock().unwrap();
        q.extend(s16le);
        const CAP: usize = 5 * 48_000 * 2 * 2;
        while q.len() > CAP {
            q.pop_front();
        }
    }

    /// Start the microphone. Uses the device's default input configuration
    /// (whatever the driver gives us — typically 44.1 / 48 kHz on iOS &
    /// macOS). Each frame is converted to s16le and forwarded to
    /// [`set_input_sink`]'s callback. Idempotent.
    pub fn start_input(&self) -> Result<(u32, u32), String> {
        if self.input.borrow().is_some() {
            return Ok((self.in_rate.get(), self.in_ch.get()));
        }
        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| "no default input device (audio) present".to_string())?;
        let supported = device
            .default_input_config()
            .map_err(|e| format!("default_input_config: {e}"))?;
        let config = supported.config();
        let (rate, ch) = (config.sample_rate.0, config.channels as u32);

        let stream = device
            .build_input_stream(
                &config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    // Read the latest sink under a brief lock (audio thread);
                    // cheap because the callback itself is expected to be
                    // "hop-and-return" — do NOT do heavy work here.
                    let slot = INPUT_SINK.get();
                    if let Some(slot) = slot {
                        if let Ok(g) = slot.lock() {
                            if let Some(cb) = g.as_ref() {
                                let bytes = pack_f32_to_s16le(data);
                                cb(&bytes, rate, ch);
                            }
                        }
                    }
                },
                |err| eprintln!("[audio] input stream error: {err}"),
                None,
            )
            .map_err(|e| format!("build_input_stream: {e}"))?;
        stream
            .play()
            .map_err(|e| format!("input stream play: {e}"))?;

        *self.input.borrow_mut() = Some(stream);
        self.in_rate.set(rate);
        self.in_ch.set(ch);
        Ok((rate, ch))
    }

    /// Start the speaker output. The render callback drains
    /// [`queue_output`]`s` FIFO (2-byte s16le samples) into cpal's f32
    /// buffer, zero-filling (silence) for any shortfall. Idempotent.
    ///
    /// Returns the negotiated (rate, channels). Consumers should produce
    /// s16le at the SAME rate/channels to avoid internal resampling.
    pub fn start_output(&self) -> Result<(u32, u32), String> {
        if self.output.borrow().is_some() {
            return Ok((self.out_rate.get(), self.out_ch.get()));
        }
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "no default output device (audio) present".to_string())?;
        let supported = device
            .default_output_config()
            .map_err(|e| format!("default_output_config: {e}"))?;
        let config = supported.config();
        let (rate, ch) = (config.sample_rate.0, config.channels as u32);
        let ch_usize = ch as usize;

        let q = self.out_queue.clone();

        let stream = device
            .build_output_stream(
                &config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let n = data.len();
                    let total = n / ch_usize; // number of frames to fill

                    let mut got_frames = 0usize;
                    {
                        let mut guard = q.lock().unwrap();
                        // Consume enough 2-byte samples (2 bytes × channels
                        // samples per frame) to cover `total` frames.
                        while got_frames < total && guard.len() >= ch_usize * 2 {
                            for c in 0..ch_usize {
                                let lo = guard.pop_front().unwrap();
                                let hi = guard.pop_front().unwrap();
                                let v = i16::from_le_bytes([lo, hi]);
                                data[got_frames * ch_usize + c] =
                                    v as f32 / 32768.0;
                            }
                            got_frames += 1;
                        }
                    }
                    // Zero-fill the rest (silence).
                    for slot in data.iter_mut().skip(got_frames * ch_usize) {
                        *slot = 0.0;
                    }
                },
                |err| eprintln!("[audio] output stream error: {err}"),
                None,
            )
            .map_err(|e| format!("build_output_stream: {e}"))?;
        stream
            .play()
            .map_err(|e| format!("output stream play: {e}"))?;

        *self.output.borrow_mut() = Some(stream);
        self.out_rate.set(rate);
        self.out_ch.set(ch);
        Ok((rate, ch))
    }

    pub fn stop_input(&self) {
        *self.input.borrow_mut() = None;
    }
    pub fn stop_output(&self) {
        *self.output.borrow_mut() = None;
    }
}

/// Install a fresh sink for incoming mic frames from the *global* slot.
/// Call before `start_input` (or the next `start_input`) for the new
/// callback to pick up.
pub fn set_input_sink(cb: impl Fn(&[u8], u32, u32) + Send + 'static) {
    *INPUT_SINK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap() = Some(Box::new(cb));
}

/// Clear the input sink (subsequent mic frames are discarded).
pub fn clear_input_sink() {
    *INPUT_SINK
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap() = None;
}

/// Resample s16le PCM from `(in_rate, in_ch)` to `(out_rate, out_ch)`.
///
/// The remote WebRTC audio track is fixed at 8 kHz mono (PCMU), but the local
/// speaker stream is negotiated at the device default (typically 48 kHz
/// stereo). Pushing the raw 8 kHz mono bytes straight into the speaker queue
/// makes the render callback misinterpret the layout (wrong sample rate →
/// pitch shift, stereo vs mono → channel corruption). This normalises the
/// payload to exactly what [`Audio::start_output`] will consume.
///
/// Strategy (voice-first, cheap): downmix the input to mono, linearly
/// resample the sample rate, then expand to the output channel count.
/// Linear interpolation is fine for telephone-band audio and keeps the render
/// callback's producer side O(n) with no allocation-heavy filtering.
///
/// No-op (returns a copy) when the layout already matches.
pub fn resample_s16le(
    input: &[u8],
    in_rate: u32,
    in_ch: u32,
    out_rate: u32,
    out_ch: u32,
) -> Vec<u8> {
    let n_in = (input.len() / (2 * in_ch.max(1) as usize)).max(0);
    if n_in == 0 {
        return Vec::new();
    }
    if in_rate == out_rate && in_ch == out_ch {
        return input.to_vec();
    }

    // 1) Downmix to mono (average over input channels).
    let mono: Vec<f32> = (0..n_in)
        .map(|i| {
            let mut acc = 0f32;
            for c in 0..in_ch as usize {
                let off = (i * in_ch as usize + c) * 2;
                let v = i16::from_le_bytes([input[off], input[off + 1]]) as f32;
                acc += v;
            }
            acc / in_ch as f32
        })
        .collect();

    let in_n = mono.len();
    let out_n = (in_n as u64 * out_rate as u64 / in_rate.max(1) as u64) as usize;
    if out_n == 0 {
        return Vec::new();
    }
    let ratio = in_n as f64 / out_n as f64;

    // 2) Linear-interpolate resample to out_rate. `mono` holds values in the
    //    i16 range (±32768), so interpolate in that range and round back.
    let resampled: Vec<i16> = (0..out_n)
        .map(|j| {
            let pos = j as f64 * ratio;
            let i0 = pos as usize;
            let frac = pos - i0 as f64;
            let a = mono[i0.min(in_n - 1)] as f64;
            let b = mono[(i0 + 1).min(in_n - 1)] as f64;
            let v = a * (1.0 - frac) + b * frac;
            (v.round().clamp(-32768.0, 32767.0)) as i16
        })
        .collect();

    // 3) Expand to out_ch.
    let mut out = Vec::with_capacity(out_n * out_ch as usize * 2);
    for s in resampled {
        for _ in 0..out_ch {
            out.extend_from_slice(&s.to_le_bytes());
        }
    }
    out
}

/// Convert an `&[f32]` (linear [-1.0, 1.0]) slice to s16le PCM bytes.
pub fn pack_f32_to_s16le(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for s in samples {
        let v = ((*s).clamp(-1.0, 1.0) * 32767.0) as i16;
        let b = v.to_le_bytes();
        out.extend_from_slice(&b);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_identity_is_copy() {
        let in_ = [0, 1, 2, 3, 4u8];
        let out = resample_s16le(&in_, 8000, 1, 8000, 1);
        assert_eq!(out, in_);
    }

    #[test]
    fn resample_rate_and_channels_length() {
        // 1 s of 8 kHz mono = 8000 samples = 16000 B. Out: 48 kHz stereo.
        let in_: Vec<u8> = (0..16000u32).map(|i| (i & 0xffff) as u8).collect();
        let out = resample_s16le(&in_, 8000, 1, 48000, 2);
        let expect = 48000 * 2 * 2; // 48000 frames * 2 ch * 2 B = 192000
        assert_eq!(out.len(), expect);
    }

    #[test]
    fn resample_dc_level_preserved() {
        // A +16k mono DC tone at 8 kHz should map (roughly) to +16k at any rate.
        let n = 8000usize;
        let in_: Vec<u8> = (0..n)
            .flat_map(|_| 16000i16.to_le_bytes())
            .collect();
        let out = resample_s16le(&in_, 8000, 1, 48000, 2);
        let v = i16::from_le_bytes([out[0], out[1]]);
        assert!((v - 16000).abs() < 64, "got {v}");
    }
}
