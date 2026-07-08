# TidaLunar

A desktop TIDAL client written in Rust with CEF (Chromium Embedded Framework) and a native audio engine (symphonia, cpal, rubato).

## Features

- FLAC and AAC/DASH streaming with adaptive buffering and next-track preloading.
- Audio cache (SQLite index, 2 GB LRU with sharded files).
- Audio device selection.
- Resampling via rubato when device sample rate differs from source.
- Optional exclusive WASAPI or ASIO output on Windows, bypassing the OS mixer.
- Media controls integration (MPRIS on Linux, SMTC and taskbar thumbnail buttons on Windows).
- Close-to-tray with system tray icon (Windows, Linux, macOS).
- In-app updater with Stable/Dev release channels (Windows, Linux).
- Plugin system: hybrid Rust + CEF execution with per-plugin sandboxing.
- Native plugin modules via Bun child process.
- Built-in plugin store with install-from-URL.

## Install

Prebuilt downloads for each release are on [GitHub Releases](https://github.com/Brskt/TidaLuna/releases/latest).

### Windows

Run the `.exe` installer (per-user, no admin prompt), or unzip the portable `.zip` and run `tidalunar.exe`.

### Linux

```bash
sudo apt install ./tidalunar_<version>_amd64.deb   # or _arm64.deb
```

Or unpack the portable `.tar.gz` and run `./tidalunar`.

### macOS

Apple Silicon only. Unzip `tidalunar-macos-arm64.zip` and run `tidalunar.app` (unsigned: right-click > Open on first launch). No in-app updater on macOS; re-download to update.

### NixOS

```bash
nix profile install github:Brskt/TidaLuna   # or: nix run github:Brskt/TidaLuna
```

## Requirements

### All platforms

- Rust (edition 2024, rustc >= 1.95)
- Bun
- CMake
- Ninja

### Linux (Ubuntu/Debian)

```bash
sudo apt-get update
sudo apt-get install -y \
  build-essential \
  pkg-config \
  libasound2-dev \
  libdbus-1-dev \
  libx11-dev \
  libglib2.0-dev \
  libgdk-pixbuf-2.0-dev \
  cmake \
  ninja-build
```

### Windows

- Visual Studio Build Tools (Desktop development with C++)
- Ninja (`choco install ninja`)

### macOS

- Xcode Command Line Tools
- CMake and Ninja (`brew install cmake ninja`)

## Build

Dev build:

```bash
cargo xtask bundle
```

Release build (optimized, strips debug symbols from CEF binaries):

```bash
cargo xtask bundle --release
```

The bundle is created in `dist/` with the executable, CEF files, and Bun runtime.

`build.rs` rebuilds the frontend (`bun esbuild.config.ts`) automatically on Linux and macOS. On Windows it reuses an existing `frontend/dist/bundle.js`, so after editing `frontend/src`, `frontend/render`, or `frontend/plugins`, run `bun esbuild.config.ts` from `frontend/` before `cargo xtask bundle`.

### Faster rebuilds (optional)

CEF's Rust bindings (`cef-dll-sys`) are large and recompile whenever the build
fingerprint changes (switching between `bundle` and `clippy`, dev vs `--release`,
or WSL vs Windows). [sccache](https://github.com/mozilla/sccache) caches the
compiler output so they build once and are reused:

```bash
cargo install sccache
export RUSTC_WRAPPER=sccache    # add to your shell profile to persist
export CARGO_INCREMENTAL=0      # lets sccache cache the workspace crate too
```

## Code quality

```bash
cargo fmt
cargo xtask clippy
```

## Logging

Logs are controlled by `LOGS` (default: 0):

- `LOGS=0` - No logs
- `LOGS=1` - General logs (IPC, player, plugins, media controls, TIDAL Connect, updater)
- `LOGS=2` - + Streaming details (governor state changes, range restarts, TTFB)
- `LOGS=3` - + Streaming verbose (chunk progress, governor periodic stats)

The in-app Settings toggle can raise the level too; `LOGS` acts as a floor over the saved value.

Linux/macOS:

```bash
LOGS=1 ./dist/tidalunar
```

Windows CMD:

```bat
set "LOGS=1" && dist\tidalunar.exe
```

## Local data path

- Linux: `~/.local/share/tidalunar`
- Windows: `%LOCALAPPDATA%\tidalunar`
- macOS: `~/.local/share/tidalunar`
