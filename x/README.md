# X — ChatX Mini-Program Extensions

Run "mini-programs" inside the main app inside a WASM sandbox: document viewers (PDF / Excel / Word / PPT), map & location, and games. A mini-program **is not running by default** — its instance is loaded on demand (e.g. when the user opens an attachment in a chat) and torn down on close.

> Status (2026-10): plan final; `x/x-m` is an empty skeleton, plugin directories and `crates/wasmhost` not yet started. Target platforms: **desktop + Android (wasmer backend) and iOS (WAMR backend)** — see §1. Related: [../README.md](../README.md) Roadmap.

## Directory layout

| Path | Role |
|---|---|
| `x/` | **Mini-program sources** (standalone cargo workspace, not merged into the main workspace) |
| `x/appsdk` | Plugin SDK: Rust bindings for the host imports (canvas / input / fs / net / geo …) + text rasterisation primitives |
| `x/pdfview` `x/sheetview` `x/docview` `x/pptview` `x/map` | The five built-in mini-programs |
| `x/x-m` | **Mini-program management server** (crate `x-manager`: listing, upload, signature verification, distribution) |
| `x-t/` | **Build artifacts** (gitignored): `<app_id>/<ver>/{manifest.json, app.wasm, assets/}` (+ optional per-platform AOT artifacts) |
| main repo `crates/wasmhost` | Host-side WASM runtime: `WasmRuntime` trait + **wasmer** backend (desktop/Android) + **WAMR** backend (iOS) + sandboxed import implementation |
| main repo `apps/chat/src/apphost.rs`, `ui/apphost.slint` | Host UI integration (AppRegistry, mini-program container page) |

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

Host code (AppRegistry, app threads, frame shipping, sandbox) is backend-agnostic and selects the implementation at startup: `wasmer` for `target_os != "ios"`, `wamr` for iOS.

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
├── lib.rs            # WasmRuntime trait + public API + backend selection
├── backend_wasmer.rs # wasmer (Cranelift) backend — desktop + Android
├── backend_wamr.rs   # WAMR C-FFI backend — iOS
├── imports.rs        # canvas / input / fs / net / geo / dialog host functions (runtime-agnostic)
├── policy.rs         # manifest parsing, permissions & memory limits, signature check (ed25519)
└── app.rs            # AppInstance lifecycle + plugin thread
```

`imports.rs` defines each host function once, as a plain entry `(context*, args…) -> i32` signature. `backend_wasmer.rs` binds those entries into wasmer import objects; `backend_wamr.rs` binds the same entries into WAMR native-function registration. One implementation of the sandbox, two thin shims.

Key implementation points:

1. **Threads**: wasmer's `Instance` is not `Sync` → **one dedicated thread per running plugin**, each with its own `Store`; a shared `Engine::new(Cranelift)` (`Clone`) avoids repeated JIT compilation. On iOS the per-plugin WAMR `wasm_runtime` is confined to its thread the same way.
2. **Lazy loading**: the runtimes themselves are statically linked (wasmer's footprint is acceptable on desktop/Android; WAMR is a few hundred KB on iOS); a *plugin instance* is loaded + instantiated only in `AppRegistry::open()`, and destroyed on close — "not running by default" holds at the instance level.
3. **No WASI directory pre-opens on either backend** — all I/O goes through the `x_`-namespaced host functions.
4. Canvas handback: the plugin draws into its wasm linear memory and calls `canvas.commit(ptr, len)`; the host reads the RGBA8 out (`memory.view(&store)` on wasmer, `wasm_runtime_addr_app_func`/memory read on WAMR) and ships the frame to the UI.
5. Limits: manifest-declared `max_memory` (wasm memory limit, enforced by each backend's config); a startup watchdog aborts an instance whose `init` exceeds the budget.

### L2 `x/appsdk` (plugin side)

```
x/appsdk/src/
├── lib.rs      # export entry: init / tick / destroy dispatch
├── canvas.rs   # resize / begin / commit(ptr,len) / dirty
├── input.rs    # poll() -> Event {Touch, Key, Resize, ...}
├── fs.rs       # read_all / list (host-supplied file + plugin private dir)
├── net.rs      # fetch(url, method, body) -> Resp (host proxies, domain allowlist)
├── geo.rs      # locate() -> {lat, lon, acc}
├── dialog.rs   # confirm / toast / pick_file(exts)
├── text.rs     # text rasterisation: fontdue + rustybuzz + unicode-linebreak (same versions as the host)
└── draw.rs     # rect / circle / line, image decode & blit (image crate)
```

**`text.rs` is the shared foundation for every document plugin**: the host's chat bubble compositor already runs on `fontdue + rustybuzz + unicode-linebreak`; reusing the same versions on the plugin side keeps CJK/latine layout behaviour consistent with the host UI. All five plugins render their text through it.

### L3 Host UI integration (main repo)

```
apps/chat/
├── src/apphost.rs     # AppRegistry (manifest index + instance pool) / open_file / open_app / close_app
├── ui/apphost.slint   # AppHostState global + container page (header with close button + Image canvas + status bar)
└── ui/components/app_state.slint  # add MiniApp to SubPageType
```

- Frame shipping: plugin thread emits `FrameReady` → `invoke_from_event_loop` → `AppHostState.set_frame(Image::from_rgba8(...))` (same `runtime().spawn + invoke_from_event_loop` pattern already used by the host).
- Teardown: hooked into the existing `on_pop_sub → release_popped_page()` — destroy = drop the runtime instance (wasmer `Store` / WAMR `wasm_runtime`) + join the thread; all wasm memory is released.
- Built-in distribution: bundled into the app package on mobile; copied to `~/.config/chatx/plugins/` on first desktop start; third-party plugins are downloaded via `x-m`, land in the same directory, and are signature-checked before use.

### Plugin ABI (host namespaces at a glance)

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

## 3. The five built-in plugins

| Plugin | Parsing / drawing deps (pure Rust, wasm-buildable) | Page logic |
|---|---|---|
| `x/pdfview` | `pdf_oxide` (pure-Rust renderer) | decode each page to a cached bitmap → canvas paging; wheel / pinch zoom |
| `x/sheetview` | `calamine` (xlsx read) | cell grid + header (draw.rs lines + text.rs text); select-and-copy export |
| `x/docview` | `opendoc` (docx paragraphs / tables / images) | block-level flowing layout (same layout params as the host); decoded images blitted |
| `x/pptview` | `pptx` crate (slide / shape / image tree) | per slide: background image + positioned text boxes + images; swipe to page |
| `x/map` | no parsing deps; `image` for tiles | OSM tile compositing + pan / zoom; `geo.locate`; `net.fetch` via allowlist |

**Scope line**: only **PDF + OOXML (docx / xlsx / pptx)**. The legacy binary `.doc/.ppt/.xls` formats have no reliable pure-Rust parser, so the manifests do not declare those extensions; when unmatched, the host shows "please open with a supporting app". Games (canvas + input + `tick` driven) can be added later as additional directories on the same appsdk surface — not in the first batch.

## 4. `x/x-m` management server

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
scripts/build_x.sh
  cd x && cargo build --release --target wasm32-wasip1 -p pdfview ...   # one artifact for all platforms
  wasm-opt -O3 → x-t/<id>/<ver>/app.wasm
  copy assets + generate manifest.json (sha256) + ed25519 signature
  tar → release package (for x-m upload / mobile bundling)

# iOS AOT (opt-in, only if interpreter throughput is insufficient for a given plugin):
  wamrc --target arm64 -o x-t/<id>/<ver>/app.aot app.wasm
```

Host-side WAMR for iOS is built once in `crates/wasmhost/build.rs` (CMake, `WAMR_BUILD_JIT=0`, `arm64-apple-ios` static library).

## 6. End-to-end paths (proof of the closed loop)

**Opening a PDF from a chat:**

```
bubble tap (chat.slint) → AppState::on_open_attachment(path)
  → apphost::open_file → Registry resolves extension to x.pdfview
  → AppRegistry::open(id, payload) ──spawn──► plugin thread:
       WasmRuntime::load(x-t/pdfview/app.wasm)      # backend: wasmer (desktop/Android) or WAMR (iOS)
       → instantiate(imports: canvas/fs/input/net…)
       → init(w, h)
  loop: tick()  [plugin: poll input → pdf_oxide decode → text/draw → canvas.commit]
       → memory_read → FrameReady
  → invoke_from_event_loop → AppHostState.frame
  close: pop_sub → release_popped_page → Registry.close → drop Store / wasm_runtime
```

**Map location:**

```
x/map calls geo.locate() → imports.rs checks policy → host asks permission
  (iOS CLLocation / Android Fused / desktop IP geolocation, via the net allowlist)
→ plugin computes tiles → net.fetch("https://tile.openstreetmap.org/…")
→ host proxies (allowlist + tile disk cache) → plugin decodes → canvas.commit
```

## 7. Security model

- **Isolation**: wasm linear memory is inherently isolated; only WASI std is opened (on both backends); no directory pre-opens — all I/O goes through the `x_` host functions gated by the manifest's `permissions`.
- **Network**: the plugin has no sockets; `net.fetch` is host-proxied with a domain allowlist (e.g. `tile.openstreetmap.org`) and a first-use consent prompt.
- **Limits**: manifest-declared `max_memory` (enforced per backend); a startup watchdog for `init`.
- **Distribution**: sha256 + ed25519 signature, public key embedded in the client; only verified plugins may run.
- **Compliance (iOS)**: no runtime JIT anywhere — WAMR is a bytecode interpreter; the optional AOT path is build-time machine code bundled as an ordinary static library.
- **Known gap**: no mature fuel mechanism against CPU spinners on either backend; shipping only signed, trusted / built-in plugins is acceptable at first, with a timeout watchdog to follow.

## 8. Milestones

| Phase | Contents | Estimate |
|---|---|---|
| **P0** | `crates/wasmhost` (`WasmRuntime` trait + **wasmer backend** + limits) + `x/appsdk` + a `hello` smoke plugin end-to-end (desktop) | 2 days |
| **P1** | `text.rs` layout foundation + `x/pdfview` + chat attachment entry + `MiniApp` subpage | 3–4 days |
| **P2** | `x/sheetview` / `x/docview` / `x/pptview` (on the shared primitives) | 3–4 days |
| **P3** | `x/map` (allowlisted net + location source incl. iOS CLLocation + tile cache) | 3 days |
| **P4** | `x/x-m` server + signature/distribution chain + "Discover / Mini-programs" list page | 2–3 days |
| **P5** | Android device regression + memory / frame-rate tuning | 2 days |
| **P6** | **iOS**: build WAMR static lib with the Apple toolchain, `backend_wamr.rs` FFI, `hello` + `x/pdfview` on device, regression; per-plugin `wamrc` AOT if interpreter throughput falls short | 3 days |

## 9. Risks

1. **`backend_wamr.rs` glue is ours to own**: WAMR's native-function registration, WASI preview1 configuration and the CMake cross-build on the Apple toolchain are the new-territory part of P6. Mitigation: it is one ~1k-LOC file behind the trait; wasmer's own tree shows WAMR builds for iOS are a solved problem.
2. **Interpreter performance on iOS**: WAMR ≈ 0.3–0.6× native; one-shot document decoding is fine, game-like plugins target 30 fps. Fallback: per-plugin `wamrc` AOT (build-time, Apple-compliant).
3. **`pdf_oxide` render quality**: complex CJK / Type3 fonts may have defects → validate with real Chinese PDFs early in P1.
4. **Pure-Rust OOXML parsing**: `calamine` / `opendoc` / `pptx` compatibility with complex templates needs per-sample regression.
5. **wasmer major-version API churn**: pin a minor version; `backend_wasmer.rs` is the single wrapper boundary to contain it.
6. **Two-runtimes maintenance**: bounded by construction — plugins/UI/server/ABI are runtime-agnostic; if wasmer's stable crate ships a `wamr`/`wasmi` backend with full fidelity, `backend_wamr.rs` can be deleted (§1.2).
