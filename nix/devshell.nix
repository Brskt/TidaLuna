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
  # Linux-only
  buildInputs = lib.optionals stdenv.isLinux [
    alsa-lib
    dbus
    libxkbcommon
    xorg.libX11
  ];
}
