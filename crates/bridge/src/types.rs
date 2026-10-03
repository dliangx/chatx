//! Fixed types for the C-ABI bridge.
//!
//! All values here are integers; they are the single stable source of truth
//! between Swift / Kotlin / Rust. Do **not** re-use or re-interpret the same
//! integer as a different concept.

/// Image pixel format identifiers.
///
/// The native shell converts to the requested format **before** calling in
/// (via AVFoundation / CameraX / MediaProjection), and the Rust consumer
/// always receives [`RGBA8888`]. The other formats are reserved for
/// future direct-passthrough paths and for the shell's internal bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum PixelFormat {
    Unknown = 0,
    /// 4 bytes per pixel: R, G, B, A (8 bits each). **Required on arrival at
    /// the Rust side.**
    Rgba8888 = 1,
    /// 3 bytes per pixel: R, G, B.
    Rgb888 = 2,
    /// Y plane + interleaved UV (NV12: U,V,U,V,...). Common on iOS `kCVPixelFormatType_420YpCbCr8BiPlanarFullRange`.
    Nv12 = 3,
    /// Y plane + interleaved VU.
    Nv21 = 4,
    /// Planar Y / U / V (I420).
    I420 = 5,
    /// Planar Y / V / U.
    Yv12 = 6,
    /// 4 bytes per pixel: B, G, R, A.
    Bgra8888 = 7,
}

impl PixelFormat {
    /// Parse an FFI-sourced integer. Returns `Unknown` on out-of-range input.
    pub fn from_i32(v: i32) -> Self {
        match v {
            0 => PixelFormat::Unknown,
            1 => PixelFormat::Rgba8888,
            2 => PixelFormat::Rgb888,
            3 => PixelFormat::Nv12,
            4 => PixelFormat::Nv21,
            5 => PixelFormat::I420,
            6 => PixelFormat::Yv12,
            7 => PixelFormat::Bgra8888,
            _ => PixelFormat::Unknown,
        }
    }

    pub fn as_i32(self) -> i32 {
        self as i32
    }
}

/// Audio sample layout constants.
///
/// Currently the only supported layout is **packed little-endian 16-bit
/// signed integers**, which is the native output of both CoreAudio
/// (`AVAudioEngine`) and Oboe / OpenSL ES for this use case. The shell MUST
/// convert to s16le before calling in; the Rust consumer always sees s16le.
pub const BIT_DEPTH_S16: i32 = 16;

/// Sample rate conventions. Values accepted by consumers are:
/// 8_000 / 16_000 / 32_000 / 44_100 / 48_000 Hz.
pub const SAMPLERATE_8K: u32 = 8_000;
pub const SAMPLERATE_16K: u32 = 16_000;
pub const SAMPLERATE_32K: u32 = 32_000;
pub const SAMPLERATE_44_1K: u32 = 44_100;
pub const SAMPLERATE_48K: u32 = 48_000;

/// Channel count conventions.
pub const CHANNELS_MONO: i32 = 1;
pub const CHANNELS_STEREO: i32 = 2;
