{
  mkShell,
  callPackage,
  rustPlatform,

  # extra tooling
  clippy,
  rustfmt,
  rust-analyzer,

  bun,
  alsa-lib,

  pkgconf,
  pkg-config,
}:
let
  defaultPackage = callPackage ./default.nix { };
in
mkShell {
  inputsFrom = [ defaultPackage ];

  env = {
    RUST_SRC_PATH = rustPlatform.rustLibSrc;
  };

  packages = [
    clippy
    rustfmt
    rust-analyzer

    alsa-lib
    bun
    pkgconf
    pkg-config
  ];
}
