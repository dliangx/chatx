<h1 align="center">ChatX</h1>

A cross-platform P2P chat application written in Rust. Private messaging, group chat, voice/video calls, screen sharing, and a social "moment" feed — with end-to-end encryption (X25519 + AES-GCM) and **no media server**: peers connect directly over libp2p (TCP / QUIC / mDNS / NAT traversal).

## Features

- **P2P messaging** — a libp2p swarm (TCP, QUIC, mDNS, UPNP, relay) for 1:1 and group chat (up to 15 members); end-to-end encryption, so the directory server never sees message contents
- **Voice & video calls** — WebRTC-based calls with H.264 video and cross-platform microphone capture
- **Screen sharing** — native screen capture on desktop, and ReplayKit / MediaProjection on iOS / Android
- **Media in chat** — images, voice messages (record and play), camera photos
- **Moments / Discover** — a post feed with photos, likes, comments and recommendations
- **Accounts & devices** — a local sealed keystore, device approval, multi-profile and autologin
- **UI** — Slint, light/dark themes, and i18n (English, 中文, 日本語, Español, Français, Русский…)
- **Storage** — local SQLite database; log file under the user's home directory

## Cross-platform

One Rust codebase, shipped to every platform:

| Platform | UI backend | Output |
|---|---|---|
| macOS | Slint on Metal | native app bundle (optionally a DMG) |
| Windows | Slint on the Windows graphics stack | a single `chatx.exe` |
| Linux | Slint | native portable build |
| Android | Slint inside a thin Kotlin host | APK |
| iOS | Slint on the Skia renderer | Simulator build or signed device build |

All OS-specific I/O (camera, mic, screen, album) is abstracted behind a platform layer; each is either a portable Rust module (desktop) or a thin native shell (mobile) — see below.

### Cross-platform architecture

Chatx follows a strict three-layer split so each layer can be swapped independently:

```
┌─────────────────────────────────────────────────────────────┐
│  Native shells (thin, optional on desktop)                  │
│  Kotlin (Android)   ·   Swift/ObjC (iOS)                    │
│  camera · mic · screen · album · permissions · UI host       │
└────────────────────────────┬────────────────────────────────┘
                              │  C boundary (JNI / ObjC bridge)
                              ▼
┌─────────────────────────────────────────────────────────────┐
│  UI layer — Slint (declarative UI, single shared source)     │
│  winit backend + renderer: Qt · Software · FemtoVG · Skia    │
└────────────────────────────┬────────────────────────────────┘
                              │  Rust API (images, callbacks)
                              ▼
┌─────────────────────────────────────────────────────────────┐
│  Core layer — pure Rust, no UI dependency                   │
│  chat core (P2P, end-to-end crypto, accounts, groups)        │
│  calls (WebRTC) · audio · camera · screen · storage          │
│  bubble compositor                                            │
└─────────────────────────────────────────────────────────────┘
```

- **Slint backends** (probed in the order Qt → winit → built-in): ChatX uses the cross-platform winit windowing layer on every OS. On desktop it renders through Skia / Metal (macOS) / OpenGL; on iOS it forces the Skia renderer so the same UI sources compile to a device build with no extra code.
- **Slint renderers**: Slint offers several interchangeable renderers (Qt, software, FemtoVG, Vello, Skia). ChatX uses **Skia** on desktop and iOS for the broadest GPU coverage, plus a custom software compositor for chat bubbles — see below.
- **Native shell boundary**: on desktop the host is the OS itself; on mobile it's a thin Kotlin/Swift shell. The only Rust↔shell interface is a plain, dependency-free C boundary, so the same ChatX core compiles into the macOS app, the Windows executable, a Linux build and the Android/iOS shared library without code changes.

### Rendering

Chatx uses **two complementary renderers** chosen per view:

| Where | What | Why |
|---|---|---|
| Whole app | **Slint + Skia** (GPU: OpenGL / Metal / Vulkan / D3D) | Layouts, inputs, transitions, i18n, accessibility hooks; single Slint source tree |
| Chat message list | **Custom software bubble compositor** | Full Unicode shaping (CJK, RTL, bidi), emoji, line-break, per-bubble caching — independent of the GPU backend |

The bubble compositor is intentionally renderer-agnostic: a small, pure-Rust module (pure-Rust glyph rasterisation, HarfBuzz-based shaping, outline parsing, line-break and bidi layout, colour-emoji runs) that turns a message into an RGBA buffer the Slint list simply blits. Benefits:

- identical text layout on every OS (no platform text-engine differences);
- no FreeType / HarfBuzz-C / ICU C dependencies — pure-Rust shaping;
- per-bubble cache with theme/version invalidation so toggling the theme doesn't re-shape every line;
- can later target **WASM** (see Roadmap).

### Size, performance & stability

| Concern | ChatX approach |
|---|---|
| **Binary size** | Release build output **69 MB** (macOS arm64) |
| **UI performance** | Slint's reactive property system (no virtual DOM, no JS garbage collector); all background → UI updates hop onto the event loop in order (no busy-polling); chat bubbles, images and avatars are cached and only re-rendered on real changes; a single async runtime is shared across the whole process |
| **Memory** | Per-thread state keeps cross-thread locks narrow; shared render state uses a single short-held mutex, so contention stays minimal even with a long message list |
| **Stability / safety** | Written in safely-checked Rust, so memory corruption and data races — the usual crash sources in C/C++ client apps — are ruled out at compile time. Failures are handled deliberately: errors are typed and propagated, user-facing messages are human-friendly while full details go to the log file, and local account data is never lost to a transient UI bug. Every native↔Rust boundary (camera, audio, screen) follows an explicit threading contract and passes only raw buffers across, so a misbehaving OS callback can't corrupt the core. The client degrades gracefully where a platform feature is missing (e.g. no capture shim) instead of failing to build or launch. |
| **Testing / CI** | Slint's GUI test automation runs the same components as the shipped app; unit tests cover the crypto, storage and text-shaping paths |

## Security

**End-to-end encryption** — messages, files, and group secrets are encrypted client-side; the directory/relay server only sees opaque ciphertext.

- **1:1 E2E**: the two devices establish a shared secret with X25519, derive a session key with HKDF, and encrypt every message with AES-GCM
- **Group E2E**: group secrets are shared through per-member encrypted envelopes, so only approved members can decrypt
- **Accounts**: the local keystore is sealed with AES-GCM using a key derived from the password (PBKDF2, with a per-user salt and iteration count). The plaintext password never hits disk in recoverable form — autologin stores only the *derived unlock key*, never the password itself
- **Device identity**: each device holds its own Ed25519 keypair and must be explicitly approved or revoked by the account holder
- **Auth & signing**: account and device records carry Ed25519 signatures; device actions such as approval and revocation flow through the signaling server

**Local data**:

- message database, keys and media live under the user's home directory, separated per profile
- a log file records full operational detail for diagnostics

## Native integration (Android / iOS)

On mobile, the native shell (Kotlin on Android, Swift/ObjC on iOS) is a thin host; Rust stays the core. The two talk through a **stable, dependency-free C boundary**:

- **Plain C ABI** — simple exported functions taking raw byte buffers and returning an integer success code; no Rust types, traits or smart pointers ever cross the bridge.
- **Flexible pixel formats** — the shell can deliver whatever its platform naturally produces (RGBA, single-channel grey, or one of the common YUV layouts) and Rust picks the right decode path.
- **Clear threading contract** — the OS capture threads (camera, microphone, screen) push frames straight into Rust on their own threads; moving results onto the UI surface is a deliberate, explicit step.

What the native shell actually does, per OS (Rust owns everything else — networking, crypto, storage, calls):

| Concern | Android (Kotlin) | iOS (Swift/ObjC) |
|---|---|---|
| Camera | Camera2 capture → bridge in | AVFoundation shim → bridge in |
| Microphone | AudioRecord → bridge in | Core Audio directly |
| Screen share | MediaProjection + virtual display → bridge in | RPScreenRecorder shim → bridge in |
| Album picker | system photo picker | PHPicker |
| Audio playback | AudioTrack ← bridge out | Core Audio directly |
| Permissions | runtime grant/reject → bridge event | system prompts |

When the app needs a platform-specific flow (e.g. to ask the user to consent to screen sharing), it asks the shell, which runs the native request on the UI thread. The bridge degrades gracefully: if a platform feature isn't available, that feature simply switches off instead of blocking the build or launch.

### Roadmap — WASM as an X extension (planned)

Slint already supports the Web platform: the UI runs on the browser via WebAssembly, drawing into a single HTML canvas through a GPU-accelerated path — see [Slint Web](https://docs.slint.dev/latest/docs/slint/guide/platforms/web/) and [Backends & Renderers](https://docs.slint.dev/latest/docs/slint/guide/backends-and-renderers/backends_and_renderers/). ChatX builds on this for a larger **"X extension" story**:

- **Web build of the client** — the chat UI (plus the platform-independent core) can be shipped to the browser for demos and lightweight access, with no native build step.
- **Sandboxed plugin runtime inside the host** — the same WebAssembly target is reused to host **user extensions ("X")**: custom message renderers and bubble themes, bot/automation logic, and third-party widgets. The host keeps full platform access (camera, microphone, storage, networking) while extension code runs inside the WebAssembly sandbox, with capability-based permissions (network, file, audio) enforced by the host rather than trusted in the plugin.
- **Why this architecture is ready for it**
  - The custom bubble compositor is renderer-agnostic, pure-Rust code, so it compiles cleanly to WebAssembly and can hand the host an RGBA buffer to blit into the UI.
  - The Rust↔shell boundary is already a plain-C surface with raw buffers and integer codes — the same shape as a WebAssembly import, so a WASM guest can slot in with no redesign.
  - The messaging/crypto/store core is platform-agnostic Rust; gating the few OS-specific pieces behind capability flags is the only change needed to produce a WASM build.
- **Open items to land**
  - an extension manifest and loader in the client
  - capability/permission scoping on the core and the native bridge (an allowlist of what a guest may call)
  - a build flag that enables the plugin runtime only for WebAssembly targets

## Quick start

### GUI client (macOS)

```sh
./scripts/build_osx.sh              # debug build
./scripts/build_osx.sh --release --dmg   # release, produces target/macos/Chatx.app + Chatx.dmg
```

### Other platforms

```sh
./scripts/build_linux.sh            # native Linux, or cross from macOS
./scripts/build_windows.ps1         # on Windows host
./scripts/build_android.sh          # Android APK (native shell)
./scripts/build_ios.sh              # iOS Simulator / device
```

### Signaling server (optional)

The client can run against the default built-in directory, or a self-hosted one:

```sh
cargo run -p chatx-signal
```

## Development

```sh
cargo build -p chatx                # build the GUI client
cargo test                          # run tests
```

- **Toolchain**: Rust; add the target for each platform you build
- **UI**: the interface is written in Slint's declarative language (one shared source across all platforms)
- **Data**: keys, logs, the message database and media all live under the user's home directory, separated per profile

## License

MIT OR Apache-2.0
