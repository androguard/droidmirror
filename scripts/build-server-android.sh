#!/usr/bin/env bash
# Cross-compile the device server and stage dex + .so under dist/android-arm64/.
# Modelled on arm_goauld/scripts/build-android.sh.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

API="$(grep '^API_LEVEL=' ndk.txt | cut -d= -f2 || true)"
API="${API:-26}"
NDK_VER="$(grep '^NDK_VERSION=' ndk.txt | cut -d= -f2 || true)"

: "${ANDROID_HOME:=${HOME}/Library/Android/sdk}"
: "${ANDROID_NDK_HOME:=${ANDROID_HOME}/ndk/${NDK_VER}}"
if [[ ! -d "$ANDROID_NDK_HOME" ]]; then
  ANDROID_NDK_HOME="$(ls -d "${ANDROID_HOME}/ndk"/* 2>/dev/null | sort -V | tail -1 || true)"
fi
[[ -d "$ANDROID_NDK_HOME" ]] || { echo "ANDROID_NDK_HOME not found" >&2; exit 1; }

PREBUILT="$(ls -d "$ANDROID_NDK_HOME"/toolchains/llvm/prebuilt/* | head -1)"
CLANG="${PREBUILT}/bin/aarch64-linux-android${API}-clang"
AR="${PREBUILT}/bin/llvm-ar"
[[ -x "$CLANG" ]] || { echo "missing clang: $CLANG" >&2; exit 1; }

export ANDROID_NDK_HOME
export CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER="$CLANG"
export CC_aarch64_linux_android="$CLANG"
export AR_aarch64_linux_android="$AR"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_AR="$AR"

if ! rustup target list --installed | grep -q '^aarch64-linux-android$'; then
  rustup target add aarch64-linux-android
fi

echo "== NDK=$ANDROID_NDK_HOME API=$API =="
cargo build -p droidmirror-server --release --target aarch64-linux-android

OUT="${ROOT}/dist/android-arm64"
mkdir -p "$OUT"
SO="${ROOT}/target/aarch64-linux-android/release/libdroidmirror_server.so"
BIN="${ROOT}/target/aarch64-linux-android/release/droidmirror-server"
[[ -f "$SO" ]] || { echo "missing $SO" >&2; exit 1; }
cp -f "$SO" "${OUT}/libdroidmirror_server.so"
cp -f "$BIN" "${OUT}/droidmirror-server"
chmod +x "${OUT}/droidmirror-server"

# Bootstrap dex. Prefer a JDK that actually runs (macOS /usr/bin/java is often a stub).
JAVA_BIN=""
if [[ -x "/Applications/Android Studio.app/Contents/jbr/Contents/Home/bin/javac" ]]; then
  export JAVA_HOME="/Applications/Android Studio.app/Contents/jbr/Contents/Home"
  JAVA_BIN="$JAVA_HOME/bin"
elif [[ -n "${JAVA_HOME:-}" && -x "${JAVA_HOME}/bin/javac" ]]; then
  JAVA_BIN="${JAVA_HOME}/bin"
fi
D8="$(ls -d "${ANDROID_HOME}/build-tools"/*/d8 2>/dev/null | sort -V | tail -1 || true)"
SRC="${ROOT}/crates/server/android"
if [[ -n "$JAVA_BIN" && -n "$D8" ]]; then
  echo "== dex via $JAVA_BIN and $D8 =="
  TMP="$(mktemp -d)"
  "$JAVA_BIN/javac" --release 8 -d "$TMP" "${SRC}/com/droidmirror/Server.java"
  (cd "$TMP" && jar cf classes.jar com/droidmirror/Server.class)
  "$D8" --min-api "$API" --output "$OUT" "$TMP/classes.jar"
  # d8 names the output classes.dex
  mv -f "${OUT}/classes.dex" "${OUT}/droidmirror.dex"
  rm -rf "$TMP"
else
  echo "javac/d8 not found; copy an existing droidmirror.dex into $OUT" >&2
  [[ -f "${OUT}/droidmirror.dex" ]] || exit 1
fi

{
  echo "git=$(git rev-parse --short HEAD 2>/dev/null || echo unknown)"
  echo "built=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
} > "${OUT}/VERSION.txt"
ls -la "$OUT"
echo "OK: $OUT"
