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
//! Callbacks run on the OS audio thread (Core Audio on iOS/macOS, Oboe on
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
