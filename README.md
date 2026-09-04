<p align="center">
  <img src="tidaluna.png" width="128" alt="TidaLunar">
</p>

<h1 align="center">TidaLunar</h1>

<p align="center">
  A native desktop TIDAL client written in Rust, bundling the <a href="https://github.com/Inrixia/TidaLuna">TidaLuna</a> client mod.
</p>

<p align="center">
  Windows &middot; Linux &middot; macOS &middot; NixOS
</p>

<p align="center">
  <a href="https://github.com/Brskt/TidaLuna/releases/latest"><img src="https://img.shields.io/github/v/release/Brskt/TidaLuna?include_prereleases&style=flat-square" alt="Latest release"></a>
  <a href="https://github.com/Brskt/TidaLuna/releases"><img src="https://img.shields.io/github/downloads/Brskt/TidaLuna/total?style=flat-square" alt="Downloads"></a>
  <a href="https://github.com/Brskt/TidaLuna/actions/workflows/ci.yml?query=branch%3Alunar-cef"><img src="https://img.shields.io/github/actions/workflow/status/Brskt/TidaLuna/ci.yml?branch=lunar-cef&style=flat-square" alt="Build"></a>
  <a href="https://discord.gg/jK3uHrJGx4"><img src="https://img.shields.io/badge/Discord-join-5865F2?style=flat-square" alt="Discord"></a>
</p>

## Features

- [TidaLuna](https://github.com/Inrixia/TidaLuna) ships built in
- Lighter and smoother
- TIDAL Connect receiver (Windows, Linux, macOS)
- Audio cache system
- ASIO output on Windows
- OS volume sync on Windows
- System media controls integration
- In-app updater
- More to come

## Roadmap

- Exclusive ALSA output on Linux
- Crossfade
- Cache settings

## Download

Every build is on [GitHub Releases](https://github.com/Brskt/TidaLuna/releases/latest).

| Platform | Formats |
| --- | --- |
| Windows | `.exe` installer (per-user, no admin prompt), portable `.zip` |
| Linux | `.deb` (amd64, arm64), portable `.tar.gz` |
| macOS | `.zip` (Apple Silicon, Intel) |
| NixOS | flake |

- **Linux:** `sudo apt install ./tidalunar_<version>_amd64.deb`, or unpack the `.tar.gz` and run `./tidalunar`.
- **macOS:** unsigned, so right-click > Open on first launch. No in-app updater; re-download to update.
- **NixOS:** `nix profile install github:Brskt/TidaLuna`, or `nix run github:Brskt/TidaLuna`.

## Support

Something not working? Come ask on the [TidaLuna Discord](https://discord.gg/jK3uHrJGx4).

## Requirements

### All platforms

- Rust (edition 2024, rustc >= 1.96)
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

---

TIDAL is a trademark of its respective owner. TidaLunar is an unofficial client,
not affiliated with or endorsed by TIDAL.
