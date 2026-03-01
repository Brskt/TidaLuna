# TidaLuna

A desktop TIDAL client written in Rust (`tao` + `wry`) with a native audio engine (`rodio`, plus optional exclusive WASAPI mode on Windows).

## Current status (important)

- The frontend is bundled with Bun during Rust builds (`build.rs`).

## Features

- FLAC streaming with adaptive buffering (Range restart, seek boost, playback/preload governor).
- Next-track preloading (RAM cache capped at `32 MB`).
- Audio device selection.
- Optional exclusive WASAPI mode on Windows, using a progressive FLAC -> PCM pipeline.
- Window management: Linux uses native decorations; Windows/macOS use frameless window controls (min/max/close) + drag/resize.
- WebView devtools via `F12`.

## Requirements

### All platforms

- Stable Rust
- Bun (required, automatically used by `build.rs`)

### Linux (Ubuntu/Debian)

```bash
sudo apt-get update
sudo apt-get install -y \
  build-essential \
  pkg-config \
  libasound2-dev \
  libgtk-3-dev \
  libwebkit2gtk-4.1-dev \
  libglib2.0-dev \
  libgdk-pixbuf-2.0-dev \
  libssl-dev
```

## Build and run

```bash
cargo run
```

Release:

```bash
cargo run --release
```

On Windows, to keep the console in release mode:

```bash
cargo run --release --features console
```

## Code quality

```bash
cargo xtask fmt
cargo xtask clippy
```

## Logging

The project uses `LOGS` for verbose logs:

- enabled by default in debug
- in release, enable explicitly

Examples:

Linux/macOS:

```bash
LOGS=1 cargo run --release
```

Windows CMD:

```bat
set LOGS=1 && cargo run --release --features console
```

Supported truthy values: `1`, `true`, `TRUE`, `yes`, `YES`, `on`, `ON`.

## Local data path

- Windows: `%LOCALAPPDATA%\\tidal-rs`
- Linux/macOS: `~/.local/share/tidal-rs`

## Notes

- User-Agent is platform-specific in `src/main.rs`.
- Bootstrap forces `TIDAL_CONFIG.enableDesktopFeatures = true` inside the WebView so desktop mode stays enabled.
