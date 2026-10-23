//! Standalone "record mic to WAV" debug hook for the in-call overlay.
//!
//! Adds a small Glyph.voice icon at the bottom-left of the control bar.
//! First click → open a fresh cpal input stream (independent of the
//! call's own mic path / `Audio` / `INPUT_SINK`), push every s16le
//! frame into a shared buffer. Second click → flush the buffer to disk
//! as `~/chatx-audio-test-<unix-ts>.wav` (16-bit PCM WAV, device rate)
//! and drop the stream.
//!
//! No existing function is touched — this module owns its own cpal
//! stream (thread-local, held on the UI thread that called `start`),
//! its own frames buffer, and writes its own file.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

/// "We are currently recording" — global so the Slint icon tint can read
/// it. Written on the UI thread, read anywhere.
static RECORDING: AtomicBool = AtomicBool::new(false);

/// Accumulated s16le PCM bytes, appended from the cpal audio thread,
/// drained by `stop()` on the UI thread. Behind an `Arc<Mutex<..>>` so
/// the cpal callback closure can take an `Arc` clone (a plain
/// `Mutex<Vec<u8>>` is not cloneable).
static FRAMES: OnceLock<Arc<Mutex<Vec<u8>>>> = OnceLock::new();

fn frames_inner() -> &'static Arc<Mutex<Vec<u8>>> {
    FRAMES.get_or_init(|| Arc::new(Mutex::new(Vec::new())))
}

/// (rate, ch) at stream start, used when stamping the WAV header on stop.
static CONFIG: Mutex<(u32, u32)> = Mutex::new((48_000, 2));

// The cpal stream handle. `cpal::Stream` is `!Send`, so a thread-local
// is the only slot that can hold it. In practice this is always the UI
// thread (the Slint callback that drives the icon runs there), so the
// `thread_local!` is effectively the same as a process-singleton.
thread_local! {
    static STREAM: std::cell::RefCell<Option<cpal::Stream>> =
        std::cell::RefCell::new(None);
}

pub fn is_recording() -> bool {
    RECORDING.load(Ordering::Relaxed)
}

/// Open a fresh cpal input stream on the UI thread. Frames accumulate in
/// `FRAMES`. The stream lives in `STREAM` (thread-local) until `stop()`.
pub fn start() -> anyhow::Result<()> {
    // Already running? No-op (icon toggle shouldn't double-start).
    if RECORDING.swap(true, Ordering::SeqCst) {
        // Reset to false so the caller doesn't think it just started
        // something that was actually already going.
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
    let config = supported.config();
    let rate = config.sample_rate.0;
    let ch = config.channels;
    *CONFIG.lock().unwrap() = (rate, ch as u32);
    eprintln!("[record] start: rate={rate} Hz, ch={ch}");

    let frames = frames_inner().clone();
    let stream = device
        .build_input_stream(
            &config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                // f32 in [-1,1] → i16 LE, appended to the shared buffer.
                // Runs on cpal's audio thread — keep it cheap.
                let mut g = frames.lock().unwrap();
                for s in data {
                    let v = (*s).clamp(-1.0, 1.0) * 32_767.0;
                    let i = v as i16;
                    g.extend_from_slice(&i.to_le_bytes());
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
    STREAM.with(|slot| {
        // If a previous recording's stream is still parked, drop it first.
        *slot.borrow_mut() = Some(stream);
    });
    Ok(())
}

/// Flush `FRAMES` to `~/chatx-audio-test-<unix-ts>.wav` and drop the
/// cpal stream. Returns the file path.
pub fn stop() -> anyhow::Result<String> {
    let was_recording = RECORDING.swap(false, Ordering::SeqCst);
    if !was_recording {
        // Still tear down (in case start() succeeded but failed halfway).
        STREAM.with(|slot| *slot.borrow_mut() = None);
        anyhow::bail!("was not recording");
    }
    STREAM.with(|slot| {
        *slot.borrow_mut() = None; // drop the handle → stream stops
    });

    let frames: Vec<u8> = std::mem::take(&mut *frames_inner().lock().unwrap());
    let (rate, ch) = *CONFIG.lock().unwrap();

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let path = format!("{home}/chatx-audio-test-{ts}.wav");
    let out = wav_encode(&frames, rate, ch);
    std::fs::write(&path, out).map_err(|e| anyhow::anyhow!("write {path}: {e}"))?;
    eprintln!("[record] wrote {path} ({} raw bytes @ {}Hz/{}ch)", frames.len(), rate, ch);
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

    // fmt sub-chunk (PCM, 16-bit).
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // format 1
    out.extend_from_slice(&ch.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample

    // data sub-chunk.
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(frames);
    out
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
