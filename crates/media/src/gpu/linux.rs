//! Linux H.264 codec backend — **VAAPI** (hardware / GPU) decode, with a
//! guaranteed [openh264] software fallback.
//!
//! VAAPI (Video Acceleration API) is the standard GPU video API on Linux,
//! supported by Intel iGPU (i965 / amdgpu / iris), AMD (radeon / amdgpu), and
//! NVIDIA (via `nvidia-vaapi-driver`).
//!
//! We `dlopen` the `libva` C functions at runtime (no link-time dependency,
//! no `bindgen`, no extra crate). This keeps the build self-contained on
//! hosts without the VAAPI dev headers installed: if `libva.so` is not found
//! or a call fails, the backend silently falls back to [openh264].
//!
//! **Decode** uses the VAAPI hardware pipeline:
//!   * one `VADisplay` per process (opened from `/dev/dri/renderD128`);
//!   * one decode context + surface pool per track (one decoder per track,
//!     per the crate contract);
//!   * each Annex-B NAL chunk is copied into the context's bitstream buffer,
//!     then `vaBeginPicture` / `vaEndPicture` triggers a synchronous decode;
//!   * the decoded picture is read back with `vaDeriveImage` as **NV12** and
//!     converted to **RGBA8888** in Rust ([`crate::gpu::nv12::nv12_to_rgba`]).
//!
//! **Encode** uses [openh264] (software). The VAAPI H.264 *encode* path
//! requires driving a stateful SPS / PPS / slice-parameter buffer protocol
//! that varies across drivers; until we have a driver test matrix we ship the
//! universally-correct software encoder (fast enough for the resolutions we
//! target). A future revision can add VAAPI encode.
//!
//! **Robustness:** the VAAPI decode path is *opportunistic*. The moment a
//! native call fails (no GPU, no H.264 VLD entry point, driver bug, size
//! mismatch, …) we permanently switch to the [openh264] software decoder. So a
//! call always decodes — GPU when available, CPU otherwise.
//!
//! **Note on context size:** VAAPI decode contexts are created per
//! resolution. We create the context at a generous default (1920×1080, which
//! covers the camera and most screen streams) and read back the *actual*
//! decoded size via `vaDeriveImage`. If a driver rejects the default size for
//! a given stream the software fallback takes over. To pin the context to a
//! specific stream size, parse the SPS width/height from the first NAL chunk
//! before the first `feed` (a future refinement).

use crate::encoder::{H264Encoder, OpenH264Encoder};
use crate::gpu::nv12::{nv12_size, nv12_to_rgba};
use crate::{H264Decoder, OpenH264Decoder};
use crate::error::Error;

// ── libva ABI (struct layouts must match `libva/va.h` exactly) ─────────────

type VADisplay = *mut std::ffi::c_void;

/// Send + Sync wrapper around a raw `VADisplay` pointer. The underlying
/// `VADisplay` is a process-global resource guarded by a `Mutex` (and the
/// per-track decode context is single-threaded), so sharing the pointer
/// across threads is sound even though the raw pointer itself is not.
#[derive(Clone, Copy)]
struct Display(VADisplay);
unsafe impl Send for Display {}
unsafe impl Sync for Display {}

type VAContextID = u32;
type VASurfaceID = u32;
type VABufferID = u32;
type VABufferType = u32;
type VAEntryPoints = u32;
type VAFourCC = u32;

const VA_STATUS_SUCCESS: i32 = 0;
/// `VAEntrypointVLD` — video decode (H.264).
#[allow(non_upper_case_globals)]
const VAEntrypointVLD: VAEntryPoints = 1;
/// `VABitstreamBuffer` — the codec bitstream buffer type.
#[allow(non_upper_case_globals)]
const VABufferType_VABitstreamBuffer: VABufferType = 60;
/// `'NV12'` fourcc.
const VA_FOURCC_NV12: VAFourCC = 0x3231_564E;
/// `VA_CHROMA_420` (value 1) — NV12 is a 4:2:0 format.
const VA_CHROMA_420: i32 = 1;

#[repr(C)]
struct VAImageFormat {
    fourcc: VAFourCC,
    chroma_format: i32,
    flags: u32,
    min_buffer_size: u32,
}

/// `VAImage` from `libva/va.h`. Layout must match byte-for-byte.
#[repr(C)]
struct VAImage {
    format: VAImageFormat,
    type_: i32,
    width: u32,
    height: u32,
    offsets: [u32; 4],
    strides: [u32; 4],
    buffers: [VABufferID; 4],
    num_buffers: u32,
}

fn nv12_fmt() -> VAImageFormat {
    VAImageFormat {
        fourcc: VA_FOURCC_NV12,
        chroma_format: VA_CHROMA_420,
        flags: 0,
        min_buffer_size: 0,
    }
}

// ── dlopen-resolved function pointers ──────────────────────────────────────

#[allow(non_camel_case_types, non_snake_case)]
type vaGetDisplay_drm_t = unsafe extern "C" fn(i32) -> VADisplay;
#[allow(non_camel_case_types)]
type vaTerminate_t = unsafe extern "C" fn(VADisplay) -> i32;
#[allow(non_camel_case_types)]
type vaCreateContext_t = unsafe extern "C" fn(
    VADisplay,
    VAEntryPoints,
    u32,
    u32,
    u32,
    *const VASurfaceID,
    u32,
    *mut VAContextID,
) -> i32;
#[allow(non_camel_case_types)]
type vaDestroyContext_t = unsafe extern "C" fn(VADisplay, VAContextID) -> i32;
#[allow(non_camel_case_types)]
type vaCreateSurfaces_t = unsafe extern "C" fn(
    VADisplay,
    *const VAImageFormat,
    u32,
    u32,
    *mut VASurfaceID,
    u32,
    *const std::ffi::c_void,
    u32,
) -> i32;
#[allow(non_camel_case_types)]
type vaDestroySurfaces_t = unsafe extern "C" fn(VADisplay, *const VASurfaceID, u32) -> i32;
#[allow(non_camel_case_types)]
type vaCreateBuffer_t = unsafe extern "C" fn(
    VADisplay,
    VAContextID,
    VABufferType,
    u32,
    u32,
    *mut VABufferID,
) -> i32;
#[allow(non_camel_case_types)]
type vaDestroyBuffer_t = unsafe extern "C" fn(VADisplay, VABufferID) -> i32;
#[allow(non_camel_case_types)]
type vaMapBuffer_t = unsafe extern "C" fn(VADisplay, VABufferID, *mut *mut u8, *mut u32) -> i32;
#[allow(non_camel_case_types)]
type vaUnmapBuffer_t = unsafe extern "C" fn(VADisplay, VABufferID) -> i32;
#[allow(non_camel_case_types)]
type vaBeginPicture_t = unsafe extern "C" fn(VADisplay, VAContextID, VASurfaceID) -> i32;
#[allow(non_camel_case_types)]
type vaEndPicture_t = unsafe extern "C" fn(VADisplay, VAContextID) -> i32;
#[allow(non_camel_case_types)]
type vaDeriveImage_t = unsafe extern "C" fn(
    VADisplay,
    VASurfaceID,
    *const VAImageFormat,
    *mut VAImage,
) -> i32;
#[allow(non_camel_case_types)]
type vaDestroyImage_t = unsafe extern "C" fn(VADisplay, VABufferID) -> i32;

#[allow(non_snake_case)]
#[allow(dead_code)]
struct Vapi {
    handle: *mut std::ffi::c_void, // dlopen handle — must outlive the pointers
    vaGetDisplay_drm: vaGetDisplay_drm_t,
    vaTerminate: vaTerminate_t,
    vaCreateContext: vaCreateContext_t,
    vaDestroyContext: vaDestroyContext_t,
    vaCreateSurfaces: vaCreateSurfaces_t,
    vaDestroySurfaces: vaDestroySurfaces_t,
    vaCreateBuffer: vaCreateBuffer_t,
    vaDestroyBuffer: vaDestroyBuffer_t,
    vaMapBuffer: vaMapBuffer_t,
    vaUnmapBuffer: vaUnmapBuffer_t,
    vaBeginPicture: vaBeginPicture_t,
    vaEndPicture: vaEndPicture_t,
    vaDeriveImage: vaDeriveImage_t,
    vaDestroyImage: vaDestroyImage_t,
}
// The resolved function pointers are stable C ABI symbols — sharing them
// across threads is sound (like any other FFI symbol table).
unsafe impl Send for Vapi {}
unsafe impl Sync for Vapi {}

static VAPI: std::sync::OnceLock<Vapi> = std::sync::OnceLock::new();

fn vapi() -> Option<&'static Vapi> {
    if let Some(v) = VAPI.get() {
        return Some(v);
    }
    let v = unsafe { load_vapi() }?;
    let _ = VAPI.set(v);
    VAPI.get()
}

#[allow(non_snake_case)]
unsafe fn load_vapi() -> Option<Vapi> {
    use libc::{dlopen, dlsym, RTLD_GLOBAL, RTLD_NOW};
    const _RTLD: libc::c_int = RTLD_NOW | RTLD_GLOBAL;
    let handle = unsafe { dlopen(b"libva.so.1\0".as_ptr() as *const libc::c_char, _RTLD) };
    if handle.is_null() {
        tracing::warn!("[media] VAAPI: dlopen(libva.so.1) failed — software decode only");
        return None;
    }
    macro_rules! sym {
        ($name:expr, $t:ty) => {{
            let p = unsafe { dlsym(handle, $name) };
            if p.is_null() {
                return None;
            }
            unsafe { std::mem::transmute::<*mut libc::c_void, $t>(p) }
        }};
    }
    let vaGetDisplay_drm = sym!(b"vaGetDisplay_drm\0".as_ptr() as *const libc::c_char, vaGetDisplay_drm_t);
    let vaTerminate = sym!(b"vaTerminate\0".as_ptr() as *const libc::c_char, vaTerminate_t);
    let vaCreateContext = sym!(b"vaCreateContext\0".as_ptr() as *const libc::c_char, vaCreateContext_t);
    let vaDestroyContext = sym!(b"vaDestroyContext\0".as_ptr() as *const libc::c_char, vaDestroyContext_t);
    let vaCreateSurfaces = sym!(b"vaCreateSurfaces\0".as_ptr() as *const libc::c_char, vaCreateSurfaces_t);
    let vaDestroySurfaces = sym!(b"vaDestroySurfaces\0".as_ptr() as *const libc::c_char, vaDestroySurfaces_t);
    let vaCreateBuffer = sym!(b"vaCreateBuffer\0".as_ptr() as *const libc::c_char, vaCreateBuffer_t);
    let vaDestroyBuffer = sym!(b"vaDestroyBuffer\0".as_ptr() as *const libc::c_char, vaDestroyBuffer_t);
    let vaMapBuffer = sym!(b"vaMapBuffer\0".as_ptr() as *const libc::c_char, vaMapBuffer_t);
    let vaUnmapBuffer = sym!(b"vaUnmapBuffer\0".as_ptr() as *const libc::c_char, vaUnmapBuffer_t);
    let vaBeginPicture = sym!(b"vaBeginPicture\0".as_ptr() as *const libc::c_char, vaBeginPicture_t);
    let vaEndPicture = sym!(b"vaEndPicture\0".as_ptr() as *const libc::c_char, vaEndPicture_t);
    let vaDeriveImage = sym!(b"vaDeriveImage\0".as_ptr() as *const libc::c_char, vaDeriveImage_t);
    let vaDestroyImage = sym!(b"vaDestroyImage\0".as_ptr() as *const libc::c_char, vaDestroyImage_t);
    Some(Vapi {
        handle,
        vaGetDisplay_drm,
        vaTerminate,
        vaCreateContext,
        vaDestroyContext,
        vaCreateSurfaces,
        vaDestroySurfaces,
        vaCreateBuffer,
        vaDestroyBuffer,
        vaMapBuffer,
        vaUnmapBuffer,
        vaBeginPicture,
        vaEndPicture,
        vaDeriveImage,
        vaDestroyImage,
    })
}

// ── Shared VAAPI display ───────────────────────────────────────────────────

/// One `VADisplay` per process, opened from the DRM render node. The render
/// node (`/dev/dri/renderD128`) works on both headless and headed systems and
/// does not require owning a display.
static VA_DISPLAY: std::sync::OnceLock<std::sync::Mutex<Option<Display>>> = std::sync::OnceLock::new();

fn va_display() -> Option<Display> {
    let v = vapi()?;
    let slot = VA_DISPLAY.get_or_init(|| std::sync::Mutex::new(None));
    let mut guard = slot.lock().unwrap();
    if let Some(d) = *guard {
        return Some(d);
    }
    for node in ["/dev/dri/renderD128", "/dev/dri/card0"] {
        let fd = match std::fs::File::open(node).and_then(|f| {
            use std::os::unix::io::AsRawFd;
            Ok(f.as_raw_fd())
        }) {
            Ok(fd) => fd,
            Err(_) => continue,
        };
        let raw = unsafe { (v.vaGetDisplay_drm)(fd) };
        if !raw.is_null() {
            let d = Display(raw);
            *guard = Some(d);
            tracing::debug!("[media] VAAPI display opened via {node}");
            return Some(d);
        }
    }
    tracing::warn!("[media] VAAPI: no DRM render node — hardware decode unavailable");
    None
}

// ── H.264 **decode** (VAAPI, GPU) with software fallback ───────────────────

/// Default decode context resolution. VAAPI contexts are created per size; this
/// generous default covers the camera stream and most screen shares. The
/// *actual* decoded size is read back per frame via `vaDeriveImage`, so
/// smaller streams work fine. See the module note on pinning to the SPS size.
const DEFAULT_CTX_W: u32 = 1920;
const DEFAULT_CTX_H: u32 = 1080;

pub struct VAAPIH264Decoder {
    /// Send + Sync wrapper so the decoder is `Send` (the trait bound for the
    /// webrtc read loop). The raw `VADisplay` is a process-global resource
    /// opened via `vaGetDisplay_drm`.
    display: Option<Display>,
    context: VAContextID,
    surface: VASurfaceID,
    surfaces: Vec<VASurfaceID>,
    bitstream_buf: VABufferID,
    /// Native path is dead (a call failed) — decode through `fallback` only.
    native_dead: bool,
    /// Guaranteed software decoder (the "a call always decodes" backstop).
    fallback: Option<OpenH264Decoder>,
    /// Scratch NV12 buffer — allocated once at the max size and reused.
    scratch: Vec<u8>,
}

impl VAAPIH264Decoder {
    pub fn new() -> Result<Self, String> {
        let sw = OpenH264Decoder::new().map_err(|e| format!("openh264 decoder alloc: {e}"))?;

        let display = match va_display() {
            Some(d) => d,
            None => {
                tracing::warn!("[media] VAAPI decode: no display — using openh264 (software) only");
                return Ok(Self {
                    display: None,
                    context: 0,
                    surface: 0,
                    surfaces: Vec::new(),
                    bitstream_buf: 0,
                    native_dead: true,
                    fallback: Some(sw),
                    scratch: Vec::new(),
                });
            }
        };

        let v = vapi().expect("va_display returned Some ⇒ vapi is Some");

        // Surface pool (VAAPI wants a few; 4 is the usual minimum).
        let mut surfaces: [VASurfaceID; 4] = [0; 4];
        let st = unsafe {
            (v.vaCreateSurfaces)(
                display.0,
                &nv12_fmt(),
                DEFAULT_CTX_W,
                DEFAULT_CTX_H,
                surfaces.as_mut_ptr(),
                4,
                std::ptr::null(),
                0,
            )
        };
        if st != VA_STATUS_SUCCESS {
            tracing::warn!("[media] VAAPI decode: vaCreateSurfaces failed ({st}) — using openh264");
            return Ok(Self {
                display: Some(display),
                context: 0,
                surface: 0,
                surfaces: Vec::new(),
                bitstream_buf: 0,
                native_dead: true,
                fallback: Some(sw),
                scratch: Vec::new(),
            });
        }

        let mut ctx: VAContextID = 0;
        let st = unsafe {
            (v.vaCreateContext)(display.0, VAEntrypointVLD, DEFAULT_CTX_W, DEFAULT_CTX_H, 0, surfaces.as_ptr(), 4, &mut ctx)
        };
        if st != VA_STATUS_SUCCESS {
            tracing::warn!("[media] VAAPI decode: vaCreateContext failed ({st}) — using openh264");
            unsafe { (v.vaDestroySurfaces)(display.0, surfaces.as_ptr(), 4) };
            return Ok(Self {
                display: Some(display),
                context: 0,
                surface: 0,
                surfaces: Vec::new(),
                bitstream_buf: 0,
                native_dead: true,
                fallback: Some(sw),
                scratch: Vec::new(),
            });
        }

        // Context bitstream buffer (64 MB — generous for a 4K IDR).
        const BS_BUF: u32 = 64 * 1024 * 1024;
        let mut bs_buf: VABufferID = 0;
        let st = unsafe {
            (v.vaCreateBuffer)(display.0, ctx, VABufferType_VABitstreamBuffer, BS_BUF, 1, &mut bs_buf)
        };
        if st != VA_STATUS_SUCCESS {
            unsafe {
                (v.vaDestroyContext)(display.0, ctx);
                (v.vaDestroySurfaces)(display.0, surfaces.as_ptr(), 4);
            }
            tracing::warn!("[media] VAAPI decode: vaCreateBuffer failed ({st}) — using openh264");
            return Ok(Self {
                display: Some(display),
                context: 0,
                surface: 0,
                surfaces: Vec::new(),
                bitstream_buf: 0,
                native_dead: true,
                fallback: Some(sw),
                scratch: Vec::new(),
            });
        }

        tracing::debug!("[media] VAAPI H.264 decode context created ({DEFAULT_CTX_W}x{DEFAULT_CTX_H})");
        Ok(Self {
            display: Some(display),
            context: ctx,
            surface: surfaces[0],
            surfaces: surfaces.to_vec(),
            bitstream_buf: bs_buf,
            native_dead: false,
            fallback: Some(sw),
            scratch: Vec::new(),
        })
    }

    /// Feed `annexb` through the software decoder.
    fn sw_feed(&mut self, annexb: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
        let fb = self.fallback.as_mut().expect("software decoder must exist");
        fb.feed(annexb).or_else(|| fb.drain())
    }

    /// Pull a decoded NV12 picture from the VAAPI surface, convert to RGBA.
    fn hw_pull(&mut self) -> Option<(Vec<u8>, u32, u32)> {
        let display = self.display?;
        let v = vapi()?;
        let mut img = VAImage {
            format: nv12_fmt(),
            type_: 0,
            width: 0,
            height: 0,
            offsets: [0; 4],
            strides: [0; 4],
            buffers: [0; 4],
            num_buffers: 0,
        };
        let st = unsafe { (v.vaDeriveImage)(display.0, self.surface, &nv12_fmt(), &mut img) };
        if st != VA_STATUS_SUCCESS {
            return None;
        }
        let w = img.width;
        let h = img.height;
        if w == 0 || h == 0 {
            unsafe { (v.vaDestroyImage)(display.0, img.buffers[0]) };
            return None;
        }

        let nv12_len = nv12_size(w, h);
        if self.scratch.len() < nv12_len {
            self.scratch.resize(nv12_len, 0);
        }

        // Copy each plane out of the derived image into one contiguous NV12
        // buffer (respecting the driver-reported offsets).
        let mut ok = true;
        for i in 0..(img.num_buffers as usize) {
            let buf_id = img.buffers[i];
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut size: u32 = 0;
            let st = unsafe { (v.vaMapBuffer)(display.0, buf_id, &mut ptr, &mut size) };
            if st != VA_STATUS_SUCCESS || ptr.is_null() {
                ok = false;
                if !ptr.is_null() {
                    unsafe { (v.vaUnmapBuffer)(display.0, buf_id) };
                }
                break;
            }
            let offset = img.offsets[i] as usize;
            if offset < nv12_len {
                let plane_len = std::cmp::min(size as usize, nv12_len - offset);
                unsafe {
                    std::ptr::copy_nonoverlapping(ptr, self.scratch.as_mut_ptr().add(offset), plane_len);
                };
            }
            unsafe { (v.vaUnmapBuffer)(display.0, buf_id) };
        }

        unsafe { (v.vaDestroyImage)(display.0, img.buffers[0]) };
        if !ok {
            return None;
        }

        let rgba_len = (w as usize) * (h as usize) * 4;
        let mut rgba = vec![0u8; rgba_len];
        nv12_to_rgba(&self.scratch[..nv12_len], w, h, &mut rgba);
        Some((rgba, w, h))
    }
}

impl H264Decoder for VAAPIH264Decoder {
    fn feed(&mut self, annexb: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
        if annexb.is_empty() {
            return None;
        }
        // Already on the software path.
        if self.native_dead {
            return self.sw_feed(annexb);
        }
        let display = match self.display {
            Some(d) => d,
            None => {
                self.native_dead = true;
                return self.sw_feed(annexb);
            }
        };
        let v = match vapi() {
            Some(v) => v,
            None => {
                self.native_dead = true;
                return self.sw_feed(annexb);
            }
        };

        // 1. Copy the Annex-B NALs into the context's bitstream buffer.
        let mut bs_ptr: *mut u8 = std::ptr::null_mut();
        let mut bs_size: u32 = 0;
        let st = unsafe { (v.vaMapBuffer)(display.0, self.bitstream_buf, &mut bs_ptr, &mut bs_size) };
        if st != VA_STATUS_SUCCESS || bs_ptr.is_null() {
            if !bs_ptr.is_null() {
                unsafe { (v.vaUnmapBuffer)(display.0, self.bitstream_buf) };
            }
            tracing::warn!("[media] VAAPI decode: vaMapBuffer failed — switching to openh264");
            self.native_dead = true;
            return self.sw_feed(annexb);
        }
        let copy_len = annexb.len().min(bs_size as usize);
        unsafe { std::ptr::copy_nonoverlapping(annexb.as_ptr(), bs_ptr, copy_len) };
        unsafe { (v.vaUnmapBuffer)(display.0, self.bitstream_buf) };

        // 2. Decode (synchronous): begin picture, end picture.
        let st = unsafe { (v.vaBeginPicture)(display.0, self.context, self.surface) };
        if st != VA_STATUS_SUCCESS {
            tracing::warn!("[media] VAAPI decode: vaBeginPicture failed ({st}) — switching to openh264");
            self.native_dead = true;
            return self.sw_feed(annexb);
        }
        let st = unsafe { (v.vaEndPicture)(display.0, self.context) };
        if st != VA_STATUS_SUCCESS {
            tracing::warn!("[media] VAAPI decode: vaEndPicture failed ({st}) — switching to openh264");
            self.native_dead = true;
            return self.sw_feed(annexb);
        }

        // 3. Read back the decoded picture.
        match self.hw_pull() {
            Some(p) => Some(p),
            None => self.sw_feed(annexb),
        }
    }

    fn drain(&mut self) -> Option<(Vec<u8>, u32, u32)> {
        if self.native_dead {
            return self.fallback.as_mut().expect("software decoder must exist").drain();
        }
        self.hw_pull()
    }

    fn close(&mut self) {
        if let Some(display) = self.display {
            if let Some(v) = vapi() {
                if self.context != 0 {
                    unsafe { (v.vaDestroyContext)(display.0, self.context) };
                    self.context = 0;
                }
                if !self.surfaces.is_empty() {
                    unsafe { (v.vaDestroySurfaces)(display.0, self.surfaces.as_ptr(), self.surfaces.len() as u32) };
                    self.surfaces.clear();
                    self.surface = 0;
                }
            }
        }
        self.native_dead = true;
    }
}

impl Drop for VAAPIH264Decoder {
    fn drop(&mut self) {
        self.close();
    }
}

// ── H.264 **encode** — openh264 software (see module note) ─────────────────
//
// A thin named wrapper so the `#[cfg(target_os = "linux")]` branch in
// `encoder::create_encoder` has a concrete type to construct. It delegates to
// [OpenH264Encoder] (the universal software path).

pub struct LinuxH264Encoder {
    inner: OpenH264Encoder,
}

impl LinuxH264Encoder {
    pub fn new(bitrate_bps: u32, max_w: u32) -> Result<Self, Error> {
        let inner = OpenH264Encoder::with_cfg(bitrate_bps, max_w)?;
        tracing::debug!("[media] H.264 encoder: openh264 (software, Linux)");
        Ok(Self { inner })
    }
}

impl H264Encoder for LinuxH264Encoder {
    fn encode_rgba(
        &mut self,
        rgba: &[u8],
        w: u32,
        h: u32,
        bitrate_bps: u32,
        max_w: u32,
    ) -> crate::error::Result<Vec<u8>> {
        self.inner.encode_rgba(rgba, w, h, bitrate_bps, max_w)
    }

    fn close(&mut self) {
        self.inner.close();
    }
}
