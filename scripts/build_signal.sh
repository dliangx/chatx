#!/usr/bin/env bash
# Chatx — chatx-signal / chatx-stun server build & packaging.
#
# Builds the x86_64 Linux server binaries and packages them into a portable
# tar.gz for deployment. Produces BOTH binaries (they ship as one package):
#
#   chatx-signal   directory + relay service   (HTTP :8080, relay :1883)
#   chatx-stun     RFC 5389 STUN binding       (UDP  :3478)
#
# Works on:
#   - macOS   (cross-compile; needs a Linux C toolchain, see below)
#   - Linux   (native x86_64 — simplest)
#
# Cross-compile from macOS requires a C compiler for the target. Install one:
#   brew tap messense/macos-cross-toolchains
#   brew install x86_64-linux-gnu          # glibc  (default)
#   brew install x86_64-linux-musl         # musl   (for --musl, static)
#
# glibc build (default) links shared libs — the target box needs its glibc
# dev libraries. musl build is self-contained (no target-host deps), best for
# minimal servers.
#
# Usage:
#   ./scripts/build_signal.sh                 # Release, x86_64-linux-gnu
#   ./scripts/build_signal.sh --musl          # musl (more portable / static)
#   ./scripts/build_signal.sh --debug         # Debug build (faster to compile)
#   ./scripts/build_signal.sh --clean         # Wipe cargo artifacts for the target first
#   ./scripts/build_signal.sh -h              # This help
#
# Output:   build/signal/chatx-signal-<target>-<mode>.tar.gz
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

RELEASE="release"
MUSL=0
CLEAN=0
OUT_DIR="$ROOT/build/signal"

while [ $# -gt 0 ]; do
  case "$1" in
    --release)  RELEASE="release" ;;
    --debug)    RELEASE="" ;;
    --musl)     MUSL=1 ;;
    --clean)    CLEAN=1 ;;
    -h|--help)  grep '^#' "$0" | sed 's/^# \?//'; exit 0 ;;
    *)          echo "unknown flag: $1" >&2; exit 2 ;;
  esac
  shift
done

MODE="${RELEASE:-debug}"
if [ "$MUSL" = "1" ]; then LIBC="musl"; else LIBC="gnu"; fi
TARGET="x86_64-unknown-linux-${LIBC}"

ON_LINUX=false
[ "$(uname -s)" = "Linux" ] && ON_LINUX=true

# ── CC / CXX / AR selection ─────────────────────────────────────────────────
# priority: $CC in env → a named cross-compiler on PATH → host cc (Linux only).
# We never auto-install anything. For musl, prefer the musl toolchain.
if [ -z "${CC:-}" ]; then
  CC=""
  FOUND=""
  if [ "$MUSL" = "1" ]; then
    # musl target must be linked by a musl toolchain — do NOT fall back to a
    # gnu-gcc (it links glibc; against a musl triple that yields a broken ELF).
    for c in "x86_64-linux-musl-gcc" "x86_64-linux-musl-clang"; do
      if command -v "$c" >/dev/null; then CC="$c"; FOUND=1; break; fi
    done
  else
    for c in "x86_64-linux-gnu-gcc" "x86_64-linux-gnu-clang" "x86_64-linux-musl-gcc"; do
      if command -v "$c" >/dev/null; then CC="$c"; FOUND=1; break; fi
    done
  fi

  # native Linux: fall back to the distro's cc (works for glibc; for --musl on
  # Linux you really need a musl toolchain — the loop above already picked one
  # or left CC empty so we can give a targeted error).
  if [ -z "$CC" ] && $ON_LINUX; then CC="cc"; fi

  if ! $ON_LINUX && { [ -z "${FOUND:-}" ] || [ -z "$CC" ]; }; then
    NEED="musl"
    [ "$MUSL" = "1" ] || NEED="glibc"
    echo "✗ no ${NEED} Linux cross-toolchain on this $(uname -s) host. Install one:" >&2
    cat >&2 <<'EOF'

    brew tap messense/macos-cross-toolchains
    brew install x86_64-linux-gnu          # glibc   (default)
    brew install x86_64-linux-musl         # musl    (for --musl)
    Then re-run.
EOF
    exit 1
  fi
fi

# Derive CXX + AR to match whatever CC we picked.
CXX=""
case "${CC}" in
  *-gcc) CXX="${CC%-gcc}-g++" ;;
  gcc)   CXX="g++" ;;
esac
AR=""
case "${CC}" in
  *-gcc) AR="${CC%-gcc}-ar" ;;
  gcc)   AR="ar" ;;
esac

# Export compilers so the build works correctly on host and target.
if $ON_LINUX; then
  # native: host == target, bare CC is correct.
  export CC
  [ -n "$CXX" ] && export CXX
  [ -n "$AR" ]  && export AR
else
  # Cross-build from a non-Linux host.
  #
  # Do NOT export bare CC/CXX/AR — cc-rs consults those for *host* build-deps
  # (e.g. `ring`'s C step), and forcing the Linux cross-compiler there yields
  # bogus Apple flags. cc-rs resolves per-target via CC_<target> (lowercase,
  # dashes→underscores), which is what we set below.
  CC_TU="$(printf '%s' "$TARGET" | tr '-' '_')"
  export "CC_${CC_TU}=$CC"
  [ -n "$CXX" ] && export "CXX_${CC_TU}=$CXX"
  [ -n "$AR" ]  && export "AR_${CC_TU}=$AR"

  # rustc still needs the cross-compiler as its linker (else it uses the host
  # `cc` — AppleClang on macOS — which can't emit a Linux ELF).
  TU="$(printf '%s' "$TARGET" | tr '[:lower:]' '[:upper:]' | tr '-' '_')"
  LINKER_NAME="$(basename "$CC")"
  export "CARGO_TARGET_${TU}_LINKER=$LINKER_NAME"

  # A cross-build must actually use a Linux toolchain, otherwise the binary
  # targeting $(uname -s) will not link against the Rust target.
  case "$CC" in
    *linux*|cc|clang|gcc|g++) : ;;
    *) echo "✗ cross-build from $(uname -s) with non-Linux cc=$CC would produce a broken ELF." >&2; exit 1 ;;
  esac
fi

echo "──────── chatx-signal build ────────"
if $ON_LINUX; then HOST_KIND="native-linux"; else HOST_KIND="cross from $(uname -s)"; fi
echo "  host   : $(uname -s)/$(uname -m) ($HOST_KIND)"
echo "  target : $TARGET"
echo "  mode   : $MODE   libc: $LIBC"
echo "  cc     : ${CC:-unset}${CXX:+   cxx: $CXX}${AR:+   ar: $AR}"

# ── rustup target ───────────────────────────────────────────────────────────
command -v cargo >/dev/null || { echo "missing tool: cargo" >&2; exit 1; }
if ! rustup target list --installed 2>/dev/null | grep -qx "$TARGET"; then
  echo "==> rustup target add $TARGET"
  rustup target add "$TARGET"
fi

# ── clean (optional) ────────────────────────────────────────────────────────
if [ "$CLEAN" -eq 1 ]; then
  echo "==> wiping $ROOT/target/$TARGET"
  rm -rf "$ROOT/target/$TARGET"
fi

# ── build (both bins come from the one chatx-signal package) ───────────────
if [ -n "$RELEASE" ]; then
  echo "==> cargo build -p chatx-signal --target $TARGET --release"
  cargo build -p chatx-signal --target "$TARGET" --release
else
  echo "==> cargo build -p chatx-signal --target $TARGET (debug)"
  cargo build -p chatx-signal --target "$TARGET"
fi
SIGNAL_BIN="$ROOT/target/$TARGET/$MODE/chatx-signal"
STUN_BIN="$ROOT/target/$TARGET/$MODE/chatx-stun"
[ -x "$SIGNAL_BIN" ] || { echo "binary not found: $SIGNAL_BIN" >&2; exit 1; }
[ -x "$STUN_BIN" ]   || { echo "binary not found: $STUN_BIN" >&2; exit 1; }

# ── package ─────────────────────────────────────────────────────────────────
echo "==> packaging $OUT_DIR"
rm -rf "$OUT_DIR"; mkdir -p "$OUT_DIR"
install -m 0755 "$SIGNAL_BIN" "$OUT_DIR/chatx-signal"
install -m 0755 "$STUN_BIN"   "$OUT_DIR/chatx-stun"

# Deployment helper: env template + optional systemd units (nice-to-have, not
# required — the binaries run with no config file of their own).
cat > "$OUT_DIR/chatx-signal.env" <<'ENV'
# copy to /etc/chatx-signal.env, edit the public host, then `source` it.
CHATX_SIGNAL_ADDR=0.0.0.0:8080
CHATX_RELAY_ADDR=0.0.0.0:1883
CHATX_RELAY_ADVERTISE=__PUBLIC_HOST__
CHATX_STUN_ADVERTISE=__PUBLIC_HOST__:3478
RUST_LOG=info
# ---- chatx-stun ----
CHATX_STUN_ADDR=0.0.0.0:3478
ENV

cat > "$OUT_DIR/chatx-signal.service" <<'UNIT'
[Unit]
Description=ChatX directory / signaling server
After=network-online.target
Wants=network-online.target

[Service]
EnvironmentFile=-/etc/chatx-signal.env
ExecStart=__BINDIR__/chatx-signal
Restart=on-failure
RestartSec=2
NoNewPrivileges=false
User=chatx-signal

[Install]
WantedBy=multi-user.target
UNIT

cat > "$OUT_DIR/chatx-stun.service" <<'UNIT'
[Unit]
Description=ChatX STUN binding server
After=network-online.target
Wants=network-online.target

[Service]
EnvironmentFile=-/etc/chatx-signal.env
ExecStart=__BINDIR__/chatx-stun
Restart=on-failure
RestartSec=2
NoNewPrivileges=true
User=chatx-signal

[Install]
WantedBy=multi-user.target
UNIT

# point the sample units at wherever the binary was installed (default: /opt/chatx)
sed -i.bak 's#__BINDIR__#/opt/chatx#' "$OUT_DIR/chatx-signal.service" "$OUT_DIR/chatx-stun.service" && rm -f "$OUT_DIR/chatx-signal.service.bak" "$OUT_DIR/chatx-stun.service.bak"

TARBALL="$OUT_DIR/chatx-signal-${TARGET/-unknown-/}-${MODE}.tar.gz"
ENTRIES=(chatx-signal chatx-stun chatx-signal.env chatx-signal.service chatx-stun.service)
tar -czf "$TARBALL" -C "$OUT_DIR" "${ENTRIES[@]}"
echo "  -> $TARBALL (items: ${ENTRIES[*]})"

# ── target-machine notes ────────────────────────────────────────────────────
echo
if [ "$MUSL" -eq 0 ]; then
  cat <<EOF
  NOTE: glibc build links shared system libraries. Target box needs, minimally:
    Debian/Ubuntu:  apt install libc6
    Fedora:         dnf install glibc
    (musl build is self-contained — most servers need nothing extra.)
EOF
else
  echo "  NOTE: musl build is self-contained; most distros need no extra libs."
fi
echo
echo "  Quick deploy (single host, public IP 203.0.113.10 as example):"
echo "    tar -xzf $TARBALL -C /opt/chatx && cd /opt/chatx"
echo "    sed -i 's/__PUBLIC_HOST__/203.0.113.10/g' /etc/chatx-signal.env 2>/dev/null || true"
echo "    (or just run with env on the command line) — see signaling/README.md"
echo
echo "──────── done ────────"
ls -lh "$TARBALL"
