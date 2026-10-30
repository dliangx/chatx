# X — ChatX Mini-Program Extensions

Run "mini-programs" inside the main app inside a WASM sandbox: document viewers (PDF / Excel / Word / PPT), map & location, and games. A mini-program **is not running by default** — its instance is loaded on demand (e.g. when the user opens an attachment in a chat) and torn down on close.

> Status (2026-10): **P0 + P1 shipped (desktop)** — `crates/wasmhost` (wasmer backend + `x` imports + WASI subset), `x/appsdk`, and the `x/hello` smoke plugin all landed; `hello` is verified end-to-end both headless (`cargo run -p wasmhost --example run_hello`) and in the chat UI full-screen overlay. **P1**: the `appsdk` `text` (fontdue) + `file` (`x_file_*`) modules, the host-document channel (`open_with_file` + `x_file_*` imports, 64 MiB cap); plus full chat-side document messaging — pdf/docx/xlsx/pptx send & receive over a new CBOR `file` channel, Slint-native file rows (icon + filename). **The `x/pdfview` viewer has been reworked back out (it was built on pdf_oxide) and is to be redone on `pdf-rs`** — rendering via `pdf-render`/`pdf-interpret` (vello_cpu, pure Rust), with **two view modes only: 宽度适配 (width-fit) and 高度适配 (height-fit)**. P2–P6 below remain plan, and `x/x-m` is just an empty crate skeleton. Target platforms: **desktop + Android (wasmer backend) and iOS (WAMR backend)** — see §1. Related: [../README.md](../README.md) Roadmap.

## Directory layout

| Path | Role |
|---|---|
| `x/` | **Mini-program sources** (standalone cargo workspace, not merged into the main workspace) |
| `x/appsdk` | Plugin SDK: Rust bindings for the host imports + text primitives. **Shipped (P0+P1):** externs + `Ctx` + 5×7 bitmap font (**P0**) + `text.rs` fontdue HUD text + `file.rs` host-document read (`x_file_*`) (**P1**); canvas/net/geo/dialog modules stay P3+ |
| `x/hello` | **P0 smoke plugin** (shipped): bouncing box + tick counter; validates the full ABI end-to-end |
| `x/pdfview` | **PDF viewer — to be redone (reworked back out).** The pdf_oxide build was reverted; it will be rebuilt on `pdf-rs` (`pdf-render`/`pdf-interpret`, vello_cpu pure-Rust rasterizer) with **two view modes only: 宽度适配 (width-fit) + 高度适配 (height-fit)**. Not currently built |
| `x/sheetview` `x/docview` `x/pptview` `x/map` | Remaining built-in mini-programs (plan, P2–P3; directories not yet created) |
| `x/x-m` | **Mini-program management server** (crate `x-manager`: listing, upload, signature verification, distribution) |
| `x-t/` | **Build artifacts** (gitignored): `<app_id>/<ver>/{manifest.json, app.wasm, assets/}` (+ optional per-platform AOT artifacts) |
| main repo `crates/wasmhost` | Host-side WASM runtime: **wasmer backend shipped (P0)** + `x_*` host functions + hand-implemented WASI preview1 subset; `WasmRuntime` trait split + **WAMR backend (iOS)** stay P6 |
| main repo `apps/chat/src/xhost.rs`, `ui/x.slint` | Host UI integration (shipped): full-screen `MiniAppOverlay` global overlay — close + minimize top-right, 16 ms frame pump |

## 1. Runtime choice: wasmer + WAMR, one ABI

- **Desktop + Android: wasmer** (Cranelift JIT backend).
- **iOS: WAMR** — the [WAMR](https://github.com/bytecodealliance/wasm-micro-runtime) C library (bytecode interpreter) statically linked into the app, driven through a thin Rust FFI wrapper.
- Both backends implement the **same trait and the same `x_` import ABI**: **every plugin is one single `app.wasm` artifact that runs unmodified on all platforms**:

```rust
pub trait WasmRuntime {
    fn load(&self, bytes: &[u8]) -> Result<Module>;
    fn instantiate(&self, m: &Module, imports: &ImportSet) -> Result<Instance>;
    fn call(&mut self, inst: &Instance, name: &str) -> Result<Value>;
    fn memory_read(&self, inst: &Instance, ptr: usize, out: &mut [u8]) -> Result<()>;
}
```

Host code (app threads, frame shipping, sandbox) is backend-agnostic and will select the implementation at startup: `wasmer` for `target_os != "ios"`, `wamr` for iOS. *(Current state: the wasmer backend is the only one and lives inline in `host.rs`; the trait + WAMR shim land in P6.)*

### 1.1 WAMR backend specifics (iOS)

- **Build**: compile WAMR from source with CMake for `arm64-apple-ios` (interpreter mode; `WAMR_BUILD_JIT=0`), produce a static `libvmlib.a` linked into the app binary. No runtime code generation anywhere.
- **Glue**: `backend_wamr.rs` wraps the WAMR C API — `wasm_runtime_instantiate`, native-function registration (host imports), `wasm_runtime_call_wasm` (call `init`/`tick`/`destroy`), memory read/write of the plugin's linear memory.
- **WASI**: enable WAMR's WASI preview1 so the plugin's `wasm32-wasip1` target runs unchanged; plugin stdout/stderr is hooked via WAMR's WASI fd redirection and logged into `chatx.log`.
- **Performance fallback**: if interpreter throughput is too low for a particular plugin, run `wamrc` **at build time** to AOT-compile that plugin's `app.wasm` into an `arm64-apple-ios` static library — this is ordinary prebuilt machine code, not runtime JIT, and remains Apple-compliant. Per-plugin, opt-in.

### 1.2 Backlog (redundancy reduction)

Track the `wasmer` crate releases: if/when the `wamr` or `wasmi` interpreter backend is exposed as a stable feature with full host-function fidelity, the iOS backend can switch from "direct WAMR glue" to "wasmer + wamr feature", deleting `backend_wamr.rs` without touching the ABI, plugins, UI or server.

## 2. Architecture layers

### L1 `crates/wasmhost` (main repo)

```
crates/wasmhost/src/
├── lib.rs              # ✓ public API (`open` / `open_with_file` + `PluginInstance`) + `Error`
├── host.rs             # ✓ wasmer backend (Cranelift): plugin thread + ~60 fps tick loop + `x_*` host functions (incl. P1 `x_file_*` document channel) — desktop (Android = P5)
├── wasi.rs             # ✓ hand-implemented WASI preview1 subset (args/environ/clock/fd_write/clocks/random/proc_exit/…; file stat/open/prestat → ENOENT; unimplemented → errno 52)
├── sandbox.rs          # ✓ backend-neutral `Sandbox` / `Frame` / `InputEvent` types (+ P1 host-supplied document slot)
└── examples/run_hello.rs
                      # ✓ headless smoke: hello (P0)
```

Planned (not split out yet):

```
├── backend_wamr.rs     # WAMR C-FFI backend — iOS (P6)
├── policy.rs           # manifest parsing, permissions & memory limits, signature check (ed25519) — P4
└── (a `WasmRuntime` trait extracted to split the two backends — §1)
```

Host functions are defined once in `host.rs` as plain `(context*, args…) -> result` handlers; the planned `backend_wamr.rs` will bind the same logic into WAMR native-function registration. One implementation of the sandbox, two thin shims.

Key implementation points:

1. **Threads**: wasmer's `Instance` is not `Sync` → **one dedicated thread per running plugin**, each with its own `Store`; a shared `Engine::new(Cranelift)` (`Clone`) avoids repeated JIT compilation. On iOS the per-plugin WAMR `wasm_runtime` is confined to its thread the same way.
2. **Lazy loading**: the runtimes themselves are statically linked (wasmer's footprint is acceptable on desktop/Android; WAMR is a few hundred KB on iOS); a *plugin instance* is loaded + instantiated only in `wasmhost::open()`, and destroyed on close — "not running by default" holds at the instance level.
3. **No WASI file access on either backend** — a minimal preview1 subset only (no pre-opens, no sockets); all app I/O goes through the `x_`-namespaced host functions.
4. Canvas handback: the plugin draws into its wasm linear memory and calls `x_frame_commit(ptr, width, height, stride)`; the host reads the RGBA8 out of linear memory (via the `memory` export on wasmer; analogous memory read on WAMR) and ships the frame to the UI. (The planned `canvas.commit` API in §2 maps onto this import.)
5. Limits (P4): manifest-declared `max_memory` (wasm memory limit, enforced by each backend's config); a startup watchdog aborts an instance whose `init` exceeds the budget. *Not yet enforced — the P0 manifest carries `max_memory`, but enforcement lands with `policy.rs`.*

### L2 `x/appsdk` (plugin side)

```
x/appsdk/src/
├── lib.rs      # ✓ host-import externs (`x_*`) + `with_ctx` + `poll_input` + `log`/`now_ms`; plugins export `init`/`tick`/`destroy` from their own crate
├── ctx.rs      # ✓ `Ctx` — RGBA8 frame buffer in plugin linear memory + `Ctx::commit` → `x_frame_commit`; P1: `blit` (page rasters) + `blend_px` (text)
├── draw.rs     # ✓ P0: 5×7 bitmap font
├── text.rs     # ✓ P1: fontdue-backed single-line text with per-char CJK fallback (feature `text`, off by default)
└── file.rs     # ✓ P1: `read_all()` / `name()` over the `x_file_*` host imports (64 MiB chunked read)
```

Planned modules (P3+, not yet split out):

```
├── canvas.rs   # resize / begin / commit(ptr,len) / dirty
├── input.rs    # poll() -> Event {Touch, Key, Resize, ...}
├── net.rs      # fetch(url, method, body) -> Resp (host proxies, domain allowlist)
├── geo.rs      # locate() -> {lat, lon, acc}
├── dialog.rs   # confirm / toast / pick_file(exts)
└── storage.rs  # per-plugin KV in the per-app data dir (iOS sandbox / `~/.config/chatx/plugins/<id>/`)
```

**`text.rs` (shipped in P1) is the text foundation for every document plugin**: a `fontdue` (0.9) single-line rasterizer with ordered per-character fallback — the same font crate the host chat bubble compositor runs, so glyph metrics match the host UI.

### L3 Host UI integration (main repo)

```
apps/chat/
├── src/xhost.rs                  # ✓ entry points: `open_hello` / `close` / `push_touch` + `Frame` → `slint::Image` conversion
├── ui/x.slint                    # ✓ `AppHost` global + full-screen `MiniAppOverlay` (Image canvas + touch area + toolbar; close & minimize top-right)
└── ui/components/app_state.slint # ✓ `GlobalOverlayType.MiniApp` — a full-screen **global overlay**, not a subpage
```

- **Entry (P0)**: the settings row or `CHATX_OPEN_X=hello` → `xhost::open_hello` → `wasmhost::open(...)` → the global overlay is shown.
- Frame shipping: **pull-based** — a 16 ms `Timer` on the overlay drives `pump-mini-app()` → `take_frame()` → `AppHost.set_frame(Image)`; no `FrameReady` queue, no `invoke_from_event_loop` for frames. (P0 loads the artifact straight from the build layout `x-t/<app_id>/<ver>/app.wasm`.)
- **Close** (top-right) = `destroy()` + drop the instance (wasmer `Store` / later WAMR `wasm_runtime`) + join the thread; all wasm memory is released.
- **Minimize** (top-right) = clear the overlay only — the instance keeps running on its thread; re-opening re-shows the existing instance without re-`init`.
- Built-in distribution (plan, P4): bundled into the app package on mobile; copied to `~/.config/chatx/plugins/` on first desktop start; third-party plugins are downloaded via `x-m`, land in the same directory, and are signature-checked before use.

### Plugin ABI

**Shipped in P0 (verified in `hello`):**

Exports — every plugin must export these from its own crate:

| Function | Signature | Notes |
|---|---|---|
| `init` | `(width: u32, height: u32, dpr_x10: u32) -> u32` | canvas pixels + device-pixel-ratio ×10; runs on the plugin thread before the first tick; return 0 = ok |
| `tick` | `(dt_ms: i64) -> u32` | ~60 fps cadence (16 ms target, 4 ms sleep slices); return 0 = ok |
| `destroy` | `() -> u32` | return 0 = ok; plugin thread exits afterwards |

Imports, namespace `x`:

| Function | Signature | Notes |
|---|---|---|
| `x_log` | `(ptr: u32, len: u32) -> u32` | log line in plugin linear memory → chatx.log |
| `x_now_ms` | `() -> i64` | host monotonic clock, ms |
| `x_frame_commit` | `(ptr: u32, width: u32, height: u32, stride: u32) -> u32` | RGBA8 frame in plugin linear memory (row-major); host copies it out to the UI |
| `x_input_poll` | `(kind_out, x_out, y_out, code_out: u32) -> i32` | plugin drains one queued event per call; kinds: `1` touch-down, `2` touch-move, `3` touch-up, `4/5` key-down/up, `16` resize |
| `x_file_size` | `() -> i64` | **P1.** size in bytes of the host-supplied document; `0` = no document for this instance |
| `x_file_read` | `(off: u64, ptr: u32, len: u32) -> i32` | **P1.** copy ≤ `len` bytes at offset `off` into plugin memory; bytes written, -1 on error |
| `x_file_name` | `(ptr: u32, cap: u32) -> i32` | **P1.** UTF-8 document basename into plugin memory (≤ `cap`, no char split); -1 when unset |

The document itself arrives through `wasmhost::open_with_file(app_id, bytes, w, h, dpr, name, file)` (P1); the host keeps one file per instance, caps it at **64 MiB** (`MAX_FILE_BYTES`), and the `file.read` manifest permission marks its use.

> Fonts: the original `x_font_*` renderer-font channel (host served real faces to the pdf_oxide build) was **reverted along with pdfview**. Fonts instead come from the PDF file itself — `pdf-interpret` (pdf-rs) extracts whatever fonts the document embeds, with its `embed-fonts` feature supplying the 14 base-14 substitutes as a fallback — so no host→sandbox font channel is needed. Re-add a channel only if a CJK font-fallback case proves it out.

Imports, namespace `wasi_snapshot_preview1` (hand-implemented in `wasi.rs`, minimal subset):

`args_get` `args_sizes_get` `environ_get` `environ_sizes_get` `clock_res_get` `clock_time_get` `fd_close` `fd_fdstat_get` `fd_seek` `fd_write` (`fd_writev`) `proc_exit` `proc_raise` `random_get` `sched_yield` — plus the file-probe surface some renderers link: `fd_filestat_get` `fd_prestat_get` `fd_prestat_dir_name` `path_open`, all answering **ENOENT** (no host files are pre-opened). Unimplemented calls fail with errno 52 (ENOSYS).

**Planned extensions** — the module-level `appsdk` surface for P1–P4 (maps onto the `x_*` imports above, e.g. `canvas.commit` = `x_frame_commit`; `text.rs` rasterisation added in P1):

| Namespace | Functions | Notes |
|---|---|---|
| `canvas` | `resize(w,h)` `commit(ptr,len)` | plugin draws into wasm linear memory; on commit the host reads it out → Slint `Image` |
| `input` | `poll() -> event` | host pushes Touch / Key / Resize; the plugin drains the queue |
| `fs` | `read_all` `list` | allowlisted: the host-supplied file + the plugin private dir |
| `net` | `fetch` | host proxies with a domain allowlist + disk cache; the plugin has no sockets |
| `geo` | `locate` | host requests the OS permission (iOS CLLocation / Android Fused / desktop IP geolocation) |
| `dialog` | `confirm` `toast` `pick_file` | native host interaction |
| `storage` | `get/put/del` | per-plugin KV in the per-app data dir (iOS sandbox / `~/.config/chatx/plugins/<id>/`) |
| `export` | `share_to_chat` `save_as` | hand a result back to the chat / save to the user dir |
| `sys` | `log` `now_ms` | logging into chatx.log |

Manifest shape:

```json
{
  "id": "x.pdfview", "name": "PDF Viewer", "version": "0.1.0",
  "extensions": ["pdf"], "icon": "assets/icon.png",
  "permissions": ["file.read", "export.share"],
  "max_memory": "256MiB", "sha256": "…", "sig": "…"
}
```

*P0 manifest (what `build_x.sh` writes to `x-t/x.hello/0.1.0/manifest.json`) already ships the first fields: `id`, `name`, `version`, `extensions`, `permissions`, `max_memory`, `sha256` — no `icon`/`sig` yet; the signature chain lands with P4 (`x/x-m` + `policy.rs`).*

## 3. The five built-in plugins

| Plugin | Parsing / drawing deps (pure Rust, wasm-buildable) | Page logic |
|---|---|---|
| `x/pdfview` | `pdf-rs` — `pdf` (parser) + `pdf-interpret` + `pdf-render` (vello_cpu pure-Rust rasterizer; **to be redone, not built**) | decode each page to a cached bitmap → canvas; **宽度适配 (width-fit)** + **高度适配 (height-fit)** modes only, tap edges or swipe to page; zoom later |
| `x/sheetview` | `calamine` (xlsx read) | cell grid + header (draw.rs lines + text.rs text); select-and-copy export |
| `x/docview` | `opendoc` (docx paragraphs / tables / images) | block-level flowing layout (same layout params as the host); decoded images blitted |
| `x/pptview` | `pptx` crate (slide / shape / image tree) | per slide: background image + positioned text boxes + images; swipe to page |
| `x/map` | no parsing deps; `image` for tiles | OSM tile compositing + pan / zoom; `geo.locate`; `net.fetch` via allowlist |

**Scope line**: only **PDF + OOXML (docx / xlsx / pptx)**. The legacy binary `.doc/.ppt/.xls` formats have no reliable pure-Rust parser, so the manifests do not declare those extensions; when unmatched, the host shows "please open with a supporting app". Games (canvas + input + `tick` driven) can be added later as additional directories on the same appsdk surface — not in the first batch.

## 4. `x/x-m` management server

*(P4 — today `x/x-m/` holds only an empty crate skeleton.)*

Reuses the `signaling/` stack (axum + sqlx-sqlite + tokio + tracing):

```sql
apps(id TEXT PK, name, icon_b64, description, created, disabled)
app_versions(app_id, version, sha256, sig, size, min_host, created, PK(app_id,version))
```

```
GET  /api/apps                      # list + latest version of each (feeds the Discover tab)
GET  /api/apps/{id}/versions
GET  /api/apps/{id}/{ver}/package   # serves the packaged artifact from x-t
POST /api/apps                      # register + upload (ed25519 signature check)
POST /api/apps/{id}/disable
```

Signature chain: the main repo's existing `ed25519-dalek` + `sha2`; verification lives in both the client (`wasmhost/policy.rs`) and the server (`x-m`); the public key is embedded in chatx.

## 5. Build pipeline (`x` → `x-t`)

```bash
scripts/build_x.sh   # ✓ shipped (P0 builds `x/hello` → x-t/x.hello/0.1.0/)
  rustup target add wasm32-wasip1
  cd x && cargo build --release --target wasm32-wasip1 -p <pkg>        # one artifact for all platforms
  copy assets/ → x-t/<id>/<ver>/assets/   (if the package ships them)
  manifest.json: id, name, version, extensions, permissions, max_memory, sha256(app.wasm)

# planned:
#   P1–P3: add the five document plugins to the built_app list
#   P4:    + ed25519 signature (wasmhost/policy.rs) + tar release package (for x-m upload / mobile bundling)
#   optional: wasm-opt -O3 over app.wasm

# iOS AOT (P6, opt-in, only if interpreter throughput is insufficient for a given plugin):
  wamrc --target arm64 -o x-t/<id>/<ver>/app.aot app.wasm
```

Host-side WAMR for iOS *(P6)* is built once in `crates/wasmhost/build.rs` (CMake, `WAMR_BUILD_JIT=0`, `arm64-apple-ios` static library).

## 6. End-to-end paths (proof of the closed loop)

**P0 smoke (`x.hello`) — shipped, verified:**

```
settings row (setting.slint)  — or  CHATX_OPEN_X=hello on launch
  → xhost::open_hello → wasmhost::open(app_id, bytes, w, h, dpr_x10)  ──spawn──► plugin thread:
       shared Engine (Cranelift) → Module → instantiate (imports: x_* + WASI subset)
       → init(w, h, dpr_x10)
  overlay 16 ms Timer → pump-mini-app() → take_frame() → AppHost.set_frame(Image)     # pull
  touch: MiniAppOverlay TouchArea → xhost::push_touch(kind, x, y) → drained by the plugin's next x_input_poll
  minimize → clear the global overlay (the instance keeps running); re-opening re-shows the same instance
  close → destroy() + join the thread + drop the Store                                   # all wasm memory released
```

**Opening a PDF from a chat (plan, P1):**

```
bubble tap (chat.slint) → AppState::on_open_attachment(path)
  → xhost::open_file → resolve extension to x.pdfview
  → wasmhost::open(id) ──spawn──► plugin thread:
       wasmer Module → instantiate(imports: x_* + WASI subset)   # P6: WAMR on iOS
       → init(w, h, dpr_x10)
  loop: tick()  [plugin: poll input → pdf-rs decode → text/draw → canvas.commit]
   → frame available; pulled each 16 ms tick: take_frame → AppHost.set_frame
  close: overlay close button → destroy() + drop Store / wasm_runtime
```

**Map location (plan, P3):**

```
x/map calls geo.locate() → host import checks policy → host asks permission
  (iOS CLLocation / Android Fused / desktop IP geolocation, via the net allowlist)
→ plugin computes tiles → net.fetch("https://tile.openstreetmap.org/…")
→ host proxies (allowlist + tile disk cache) → plugin decodes → canvas.commit
```

## 7. Security model

- **Isolation**: wasm linear memory is inherently isolated; what is opened is a minimal, hand-implemented WASI preview1 subset (args/environ/clock/`fd_write`/`fd_seek`/`proc_exit`/`random_get`/`sched_yield` — **no pre-opens, no sockets**; plus the file-probe surface plugins link — `path_open`/stat/prestat — all answering ENOENT to force bundled-font fallbacks), not the full `wasi` crate; unimplemented calls fail with errno 52 (ENOSYS). All app I/O today goes through the `x_*` host functions — including the one host-supplied document (`x_file_*`, 64 MiB cap, `file.read` manifest permission); the wider `x` namespaces of §2 will be gated by the manifest's `permissions` when they land (P4).
- **Network**: the plugin has no sockets; `net.fetch` is host-proxied with a domain allowlist (e.g. `tile.openstreetmap.org`) and a first-use consent prompt.
- **Limits** (P4): the manifest already declares `max_memory`, but enforcement (wasm memory limit per backend + a startup watchdog for `init`) lands with `policy.rs`.
- **Distribution**: sha256 of `app.wasm` is already part of the manifest (`build_x.sh`); the ed25519 signature + public key embedded in the client is P4 — until then only built-in/`hello`-class artifacts are shipped.
- **Compliance (iOS)**: no runtime JIT anywhere — WAMR is a bytecode interpreter; the optional AOT path is build-time machine code bundled as an ordinary static library.
- **Known gap**: no mature fuel mechanism against CPU spinners on either backend; shipping only signed, trusted / built-in plugins is acceptable at first, with a timeout watchdog to follow.

## 8. Milestones

| Phase | Contents | Estimate |
|---|---|---|
| **P0** ✅ shipped | `crates/wasmhost` (**wasmer backend** + `x_*` imports + WASI subset, `policy.rs` limits deferred to P4) + `x/appsdk` + `x/hello` smoke plugin end-to-end on desktop; verified headless and in the full-screen chat overlay | — |
| **P1** | `text.rs` foundation + `x_file_*` host-document channel + chat-side document messaging send/receive + file bubbles (open flow from chat). **The `x/pdfview` viewer itself was reworked back out (pdf_oxide) and is scheduled to be redone on `pdf-rs` (宽度适配 / 高度适配) as the next step** | **shipped (viewer pending pdf-rs)** |
| **P2** | `x/sheetview` / `x/docview` / `x/pptview` (on the shared primitives) | 3–4 days |
| **P3** | `x/map` (allowlisted net + location source incl. iOS CLLocation + tile cache) | 3 days |
| **P4** | `x/x-m` server + signature/distribution chain + "Discover / Mini-programs" list page | 2–3 days |
| **P5** | Android device regression + memory / frame-rate tuning | 2 days |
| **P6** | **iOS**: build WAMR static lib with the Apple toolchain, `backend_wamr.rs` FFI, `hello` (+ `x/pdfview` once it lands on pdf-rs) on device, regression; per-plugin `wamrc` AOT if interpreter throughput falls short | 3 days |

## 9. Risks

1. **`backend_wamr.rs` glue is ours to own**: WAMR's native-function registration, WASI preview1 configuration and the CMake cross-build on the Apple toolchain are the new-territory part of P6. Mitigation: it is one ~1k-LOC file behind the trait; wasmer's own tree shows WAMR builds for iOS are a solved problem.
2. **Interpreter performance on iOS**: WAMR ≈ 0.3–0.6× native; one-shot document decoding is fine, game-like plugins target 30 fps. Fallback: per-plugin `wamrc` AOT (build-time, Apple-compliant).
3. **`pdf-rs` render quality**: CJK coverage (the embedded base-14 set won't cover it — rely on the document's embedded/subset fonts via `pdf-interpret`) and complex Type3 fonts → validate with real Chinese PDFs early. Also confirm `pdf-render` (vello_cpu) is wasm32-wasip1 compatible before committing to it.
4. **Pure-Rust OOXML parsing**: `calamine` / `opendoc` / `pptx` compatibility with complex templates needs per-sample regression.
5. **wasmer major-version API churn**: the root `Cargo.toml` pins `wasmer = "7.2.1"` (lockfile currently on 7.5.0); `host.rs` is the single API boundary to contain it.
6. **Two-runtimes maintenance**: bounded by construction — plugins/UI/server/ABI are runtime-agnostic; if wasmer's stable crate ships a `wamr`/`wasmi` backend with full fidelity, `backend_wamr.rs` can be deleted (§1.2).
