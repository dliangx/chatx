//! "Record mic to PCM" + playback helper used by the voice-message feature.
//!
//! The legacy in-call debug hook (`start` / `stop`) is preserved so the
//! existing `call::toggle-record` callback keeps working. The voice-message
//! UI uses `start_capture` / `stop_capture` which return the raw PCM bytes
//! instead of writing a WAV file to disc — the caller decides what to do
//! with them (send them over P2P, play them back, etc.).
//!
//! Playback is a thin wrapper around [`audio::Audio`] with a global
//! playing-state flag so the Slint UI can flip a spinner.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

static RECORDING: AtomicBool = AtomicBool::new(false);
static FRAMES: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();

/// Hard cap on captured PCM bytes: ~5 min at 48 kHz / 2 ch / 16-bit.
/// The audio callback stops appending when this is reached, preventing
/// unbounded memory growth if the user forgets to tap stop.
/// 5 min × 48000 Hz × 2 ch × 2 bytes = 4 800 000 bytes ≈ 5 MB.
pub const MAX_FRAMES: usize = 5 * 60 * 48_000 * 2 * 2;

struct StreamCfg {
    rate: u32,
    ch: u32,
}
impl Clone for StreamCfg {
    fn clone(&self) -> Self {
        Self {
            rate: self.rate,
            ch: self.ch,
        }
    }
}
static CONFIG: Mutex<StreamCfg> = Mutex::new(StreamCfg { rate: 48_000, ch: 2 });

thread_local! {
    static STREAM: std::cell::RefCell<Option<cpal::Stream>> =
        std::cell::RefCell::new(None);
}

fn frames_inner() -> &'static Arc<Mutex<Vec<u8>>> {
    FRAMES.get_or_init(|| Arc::new(Mutex::new(Vec::new())))
}

/// Begin capturing mic PCM. No-op (error) if already recording.
pub fn start() -> anyhow::Result<()> {
    if RECORDING.swap(true, Ordering::SeqCst) {
        RECORDING.store(false, Ordering::SeqCst);
        anyhow::bail!("already recording");
    }
    frames_inner().lock().unwrap().clear();

    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| anyhow::anyhow!("no default input device"))?;
    let supported = device
        .default_input_config()
        .map_err(|e| anyhow::anyhow!("default_input_config: {e}"))?;
    let cfg = supported.config();
    let rate = cfg.sample_rate.0;
    let ch = cfg.channels;
    *CONFIG.lock().unwrap() = StreamCfg {
        rate,
        ch: ch as u32,
    };
    eprintln!("[record] start: rate={rate} Hz, ch={ch}");

    let frames = frames_inner().clone();
    let stream = device
        .build_input_stream(
            &cfg,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                let mut g = frames.lock().unwrap();
                let room = MAX_FRAMES.saturating_sub(g.len());
                if room == 0 {
                    return; // cap reached: stop buffering (memory is bounded)
                }
                for s in data.iter().take(room / 2) {
                    let v = (*s).clamp(-1.0, 1.0) * 32_767.0;
                    let i = v as i16;
                    g.extend_from_slice(&i.to_le_bytes());
                    if g.len() >= MAX_FRAMES {
                        break;
                    }
                }
            },
            |err| eprintln!("[record] input stream error: {err}"),
            None,
        )
        .map_err(|e| anyhow::anyhow!("build_input_stream: {e}"))?;
    stream
        .play()
        .map_err(|e| {
            RECORDING.store(false, Ordering::SeqCst);
            anyhow::anyhow!("play: {e}")
        })?;
    STREAM.with(|slot| *slot.borrow_mut() = Some(stream));
    Ok(())
}

/// Result of a captured voice clip.
#[derive(Debug, Clone)]
pub struct RecordedVoice {
    /// s16le PCM bytes (frames × channels × 2).
    pub data: Vec<u8>,
    /// Sample rate in Hz at capture time.
    pub rate: u32,
    /// Channel count (1 or 2).
    pub ch: u32,
    /// Accumulated capture duration in milliseconds (frames / rate × 1000).
    pub duration_ms: u64,
}

/// True if a capture stream is currently open on this thread.
pub fn is_recording() -> bool {
    RECORDING.load(Ordering::Relaxed)
}

static T0: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

/// Monotonic milliseconds since the process started (for the elapsed label).
pub fn monotonic_ms() -> u64 {
    let t0 = T0.get_or_init(Instant::now);
    t0.elapsed().as_millis() as u64
}

/// Stop the stream and return the raw PCM bytes (not written to disk).
///
/// The returned `duration_ms` is computed from the sample count and the
/// sample rate — it matches what the capture device clocked.
pub fn stop_capture() -> anyhow::Result<RecordedVoice> {
    let was_recording = RECORDING.swap(false, Ordering::SeqCst);
    if !was_recording {
        STREAM.with(|slot| *slot.borrow_mut() = None);
        anyhow::bail!("was not recording");
    }
    STREAM.with(|slot| *slot.borrow_mut() = None);
    let data: Vec<u8> = std::mem::take(&mut *frames_inner().lock().unwrap());
    let c = (*CONFIG.lock().unwrap()).clone();
    let samples = (data.len() / (2 * c.ch.max(1) as usize)).max(0) as u64;
    let duration_ms = if c.rate == 0 {
        0
    } else {
        (samples * 1000) / c.rate as u64
    };
    Ok(RecordedVoice {
        data,
        rate: c.rate,
        ch: c.ch,
        duration_ms,
    })
}

/// Compatibility shim: stop + persist a WAV file to $HOME.
pub fn stop() -> anyhow::Result<String> {
    let rec = stop_capture()?;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let path = format!("{home}/chatx-audio-test-{ts}.wav");
    std::fs::write(&path, wav_encode(&rec.data, rec.rate, rec.ch))
        .map_err(|e| anyhow::anyhow!("write {path}: {e}"))?;
    eprintln!(
        "[record] wrote {path} ({} raw bytes @ {}Hz/{}ch, {}ms)",
        rec.data.len(),
        rec.rate,
        rec.ch,
        rec.duration_ms
    );
    Ok(path)
}

/// Minimal 16-bit PCM WAV encoder (44-byte RIFF header + raw s16le bytes).
fn wav_encode(frames: &[u8], rate: u32, ch: u32) -> Vec<u8> {
    let ch = (ch as u16).max(1);
    let data_len = frames.len() as u32;
    let byte_rate = rate * (ch as u32) * 2;
    let block_align = ch;

    let mut out = Vec::with_capacity(44 + frames.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36u32 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");

    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&ch.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());

    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(frames);
    out
}

// ─────────────────────────────────────────────────────────────────────────────
// Playback
// ─────────────────────────────────────────────────────────────────────────────

// The speaker stream. `Audio` is `!Send` (thread-local cpal streams), and all
// our playback entry points run on the UI thread, so a thread-local fits.
thread_local! {
    static P_AUDIO: std::cell::RefCell<Option<audio::Audio>> =
        std::cell::RefCell::new(None);
}

static PLAYING: AtomicBool = AtomicBool::new(false);

pub fn voice_dir() -> std::path::PathBuf {
    let dir = chatx_core::identity::DeviceIdentity::home_dir().join("voice");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Self-describing voice-clip container: `"CHAV"` + `rate:u32le` + `ch:u32le`
/// + raw s16le PCM. Storing the sample rate + channel count in the file lets
/// the receiver resample correctly regardless of which device recorded it.
pub fn write_voice_clip(
    rate: u32,
    ch: u32,
    data: &[u8],
) -> anyhow::Result<std::path::PathBuf> {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dir = voice_dir();
    let path = dir.join(format!("v-{ts}.pcm"));
    let mut out = Vec::with_capacity(12 + data.len());
    out.extend_from_slice(b"CHAV");
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&ch.to_le_bytes());
    out.extend_from_slice(data);
    std::fs::write(&path, out).map_err(|e| anyhow::anyhow!("write {path:?}: {e}"))?;
    Ok(path)
}

/// Read a clip written by [`write_voice_clip`] → `(data, rate, ch)`.
pub fn read_voice_clip(path: &str) -> anyhow::Result<(Vec<u8>, u32, u32)> {
    let buf = std::fs::read(path).map_err(|e| anyhow::anyhow!("read {path}: {e}"))?;
    if buf.len() < 12 || &buf[0..4] != b"CHAV" {
        anyhow::bail!("bad voice clip {path}");
    }
    let rate = u32::from_le_bytes(buf[4..8].try_into().unwrap());
    let ch = u32::from_le_bytes(buf[8..12].try_into().unwrap());
    Ok((buf[12..].to_vec(), rate, ch.max(1)))
}


/// Queue `s16le` bytes (at `rate`/`ch`) for speaker playback and start
/// the output stream. Safe to call at any time — idempotent.
///
/// The returned value is the clip's duration in ms (from the *source*
/// `rate`/`ch`), which the UI uses for the elapsed timer.
pub fn play(data: Vec<u8>, rate: u32, ch: u32) -> anyhow::Result<u64> {
    // Compute the duration while we still own `data` (it's moved into the
    // queue below), so we don't need a copy of the whole clip.
    let samples = (data.len() / (2 * ch.max(1) as usize)).max(0) as u64;
    let duration_ms = if rate == 0 {
        0
    } else {
        (samples * 1000) / rate as u64
    };
    let (out_rate, out_ch) = P_AUDIO.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(audio::Audio::new());
        }
        slot.as_mut().unwrap().start_output()
    })
    .map_err(|e| anyhow::anyhow!("start_output: {e}"))?;
    let s16le = if rate != out_rate || ch != out_ch {
        audio::resample_s16le(&data, rate, ch, out_rate, out_ch)
    } else {
        // Same layout: hand the buffer to the queue without copying it.
        data
    };
    P_AUDIO.with(|cell| {
        cell.borrow_mut()
            .as_mut()
            .unwrap()
            .queue_output(s16le)
    });
    PLAYING.store(true, Ordering::SeqCst);
    Ok(duration_ms)
}

/// Immediately stop speaker output (drops any queued samples).
pub fn stop_playback() {
    PLAYING.store(false, Ordering::SeqCst);
    P_AUDIO.with(|cell| {
        if let Some(a) = cell.borrow_mut().as_mut() {
            a.stop_output();
        }
    });
}

pub fn is_playing() -> bool {
    PLAYING.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_layout() {
        let pcm: Vec<u8> = (0..64u32).flat_map(|i| (i as i16).to_le_bytes()).collect();
        let out = wav_encode(&pcm, 48_000, 2);
        assert_eq!(&out[0..4], b"RIFF");
        assert_eq!(&out[4..8], &(36u32 + pcm.len() as u32).to_le_bytes());
        assert_eq!(&out[8..12], b"WAVE");
        assert_eq!(&out[12..16], b"fmt ");
        assert_eq!(out.len(), 44 + pcm.len());
    }
}
