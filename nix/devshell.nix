{
  lib,
  stdenv,
  mkShell,
  rustc,
  cargo,
  clippy,
  rustfmt,
  rust-analyzer,
  bun,
  cmake,
  ninja,
  pkg-config,
  alsa-lib,
  dbus,
  libxkbcommon,
  xorg,
}:
mkShell {
  # Project requires rustc >= 1.95 (oxc_transformer / if_let_guard).
  nativeBuildInputs = [
    rustc
    cargo
    clippy
    rustfmt
    rust-analyzer
    bun
    cmake
    ninja
    pkg-config
  ];
  # alsa-lib does not exist for darwin at all; the rest are equally Linux-only.
  buildInputs = lib.optionals stdenv.hostPlatform.isLinux [
    alsa-lib
    dbus
    libxkbcommon
    xorg.libX11
  ];
}
