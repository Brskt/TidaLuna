# TidaLuna

A desktop TIDAL client written in Rust with CEF (Chromium Embedded Framework) and a native audio engine (symphonia, cpal, rubato).

## Features

- FLAC and AAC/DASH streaming with adaptive buffering and next-track preloading.
- Audio cache (SQLite index, 2 GB LRU with sharded files).
- Audio device selection.
- Resampling via rubato when device sample rate differs from source.
- Optional exclusive WASAPI mode on Windows (progressive FLAC/AAC to PCM pipeline).
- Media controls integration (MPRIS on Linux, thumbnail toolbar on Windows).
- Plugin system: hybrid Rust + CEF execution with per-plugin sandboxing.
- Native plugin modules via Bun child process.

## Requirements

### All platforms

- Stable Rust
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
  libgtk-3-dev \
  libglib2.0-dev \
  libgdk-pixbuf-2.0-dev \
  cmake \
  ninja-build
```

### Windows

- Ninja (`choco install ninja`)

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

## Code quality

```bash
cargo xtask fmt
cargo xtask clippy
```

## Logging

Logs are controlled by `LOGS` (level 1 enabled by default in debug builds):

- `LOGS=0` — No logs
- `LOGS=1` — General logs (IPC, player, plugins, media controls)
- `LOGS=2` — + Streaming details (governor state changes, range restarts, TTFB)
- `LOGS=3` — + Streaming verbose (chunk progress, governor periodic stats)

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
- macOS: `~/Library/Application Support/tidalunar`
