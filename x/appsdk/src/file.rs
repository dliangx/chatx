//! Host-supplied document access (`x_file_*` imports, P1).
//!
//! The sandbox has **no** WASI pre-opens — plugins cannot touch the filesystem
//! directly. Instead the host hands the instance exactly one file at
//! `open()` time (see `wasmhost::open_with_file`); these calls read it in
//! chunks from the host.

#[link(wasm_import_module = "x")]
unsafe extern "C" {
    /// Size of the host-supplied document in bytes; **0 = no file** for this
    /// instance.
    pub fn x_file_size() -> i64;
    /// Copy up to `len` bytes of the document starting at byte `off` into
    /// plugin linear memory at `buf`. Returns bytes written, -1 on error.
    pub fn x_file_read(off: u64, buf: u32, len: u32) -> i32;
    /// UTF-8 document basename into plugin linear memory (up to `cap`
    /// bytes). Returns bytes written, -1 when the instance has no document
    /// name.
    pub fn x_file_name(buf: u32, cap: u32) -> i32;
}

/// Hard read cap so a misbehaving manifest can't drive the plugin heap;
/// mirrors the host-side `open_with_file` ceiling.
const MAX_DOC_BYTES: usize = 64 * 1024 * 1024;

const CHUNK: usize = 256 * 1024;

/// The complete host-supplied document, or `None` when the instance was
/// opened without one.
pub fn read_all() -> Option<Vec<u8>> {
    let total = unsafe { x_file_size() };
    if total <= 0 {
        return None;
    }
    let total = (total as usize).min(MAX_DOC_BYTES);
    let mut chunk = vec![0u8; CHUNK.min(total)];
    let mut out: Vec<u8> = Vec::with_capacity(total.min(16 * 1024 * 1024));
    let mut off: u64 = 0;
    while (off as usize) < total {
        let want = (total - off as usize).min(chunk.len());
        let n = unsafe { x_file_read(off, chunk.as_mut_ptr() as u32, want as u32) };
        if n <= 0 {
            // -1 = host error (bad offset/memory); 0 = unexpected EOF. Either
            // way the document is incomplete — a truncated PDF would only fail
            // to parse, so surface the error straight away.
            return None;
        }
        let n = n as usize;
        out.extend_from_slice(&chunk[..n]);
        off += n as u64;
    }
    Some(out)
}

/// The host-supplied document basename (e.g. `报告.pdf`), or `None`.
pub fn name() -> Option<String> {
    const CAP: usize = 512;
    let mut buf = vec![0u8; CAP];
    let n = unsafe { x_file_name(buf.as_mut_ptr() as u32, CAP as u32) };
    if n <= 0 {
        return None;
    }
    let n = (n as usize).min(CAP);
    let s = String::from_utf8_lossy(&buf[..n]).into_owned();
    (!s.is_empty()).then_some(s)
}
