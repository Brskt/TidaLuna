{
  lib,
  stdenv,
  fetchurl,
  autoPatchelfHook,
  makeWrapper,
  makeDesktopItem,
  copyDesktopItems,
  nss,
  nspr,
  gtk3,
  glib,
  atk,
  at-spi2-atk,
  at-spi2-core,
  cups,
  libdrm,
  mesa,
  alsa-lib,
  dbus,
  pango,
  cairo,
  gdk-pixbuf,
  libxkbcommon,
  libGL,
  expat,
  fontconfig,
  freetype,
  zlib,
  zstd,
  xorg,
}:

let
  version = "0.0.10-alpha";

  # Maps each Nix system to its release-asset arch token and the tarball sha256.
  # VERSION BUMP: bump `version`, then `nix store prefetch-file <url>` per arch
  # (arch-neutral) and paste the printed `sha256-...` into the matching `hash`.
  arches = {
    "x86_64-linux" = {
      arch = "amd64";
      hash = "sha256-nYjBm1xQUTNrBFEsjLxfB7EDPkP5RrieoPEtynCvQyc=";
    };
    "aarch64-linux" = {
      arch = "arm64";
      hash = "sha256-iibjniAnQwWVT7wNRhRP+1XyHWjQN77kfTYLmUVxQD8=";
    };
  };

  selected =
    arches.${stdenv.hostPlatform.system}
      or (throw "tidalunar: unsupported system ${stdenv.hostPlatform.system}");
in
stdenv.mkDerivation {
  pname = "tidalunar";
  inherit version;

  src = fetchurl {
    url =
      "https://github.com/Brskt/TidaLuna/releases/download/"
      + "${version}/tidalunar_${version}_linux_${selected.arch}.tar.gz";
    inherit (selected) hash;
  };

  nativeBuildInputs = [
    autoPatchelfHook
    makeWrapper
    copyDesktopItems
  ];

  buildInputs = [
    nss
    nspr
    gtk3
    glib
    atk
    at-spi2-atk
    at-spi2-core
    cups
    libdrm
    mesa
    alsa-lib
    dbus
    pango
    cairo
    gdk-pixbuf
    libxkbcommon
    libGL
    expat
    fontconfig
    freetype
    zlib
    zstd
    xorg.libX11
    xorg.libXrandr
    xorg.libXcomposite
    xorg.libXdamage
    xorg.libXfixes
    xorg.libXext
    xorg.libXrender
    xorg.libxcb
  ];

  # Chromium dlopen()s these; they aren't NEEDED entries: add them to the runpath.
  runtimeDependencies = [
    libGL
    mesa
  ];

  dontConfigure = true;
  dontBuild = true;

  # Keep `tidalunar` next to `bin/cef/` for its $ORIGIN/bin/cef RPATH to resolve
  # libcef.so.
  installPhase = ''
    runHook preInstall

    mkdir -p "$out/libexec/tidalunar"
    cp -r . "$out/libexec/tidalunar/"

    mkdir -p "$out/bin"
    # Chromium dlopen()s libGL.so.1 from libcef.so, and DT_RUNPATH doesn't apply
    # to dlopen; put it (+ the NixOS GPU driver) on LD_LIBRARY_PATH.
    # TIDALUNAR_MANAGED_INSTALL makes the app skip its desktop self-install and
    # in-app updater (Nix owns both). Inert until a release carries the gate.
    makeWrapper "$out/libexec/tidalunar/tidalunar" "$out/bin/tidalunar" \
      --prefix LD_LIBRARY_PATH : "/run/opengl-driver/lib:${lib.makeLibraryPath [ libGL ]}" \
      --set TIDALUNAR_MANAGED_INSTALL 1

    # Icons are committed in-repo (not shipped in the release tarball).
    for png in ${../installer/linux/deb/icons/hicolor}/*/apps/tidalunar.png; do
      sizedir=$(basename "$(dirname "$(dirname "$png")")")
      install -Dm644 "$png" "$out/share/icons/hicolor/$sizedir/apps/tidalunar.png"
    done

    runHook postInstall
  '';

  desktopItems = [
    (makeDesktopItem {
      name = "tidalunar";
      desktopName = "TidaLunar";
      exec = "tidalunar";
      icon = "tidalunar";
      comment = "A TIDAL client";
      categories = [
        "AudioVideo"
        "Audio"
        "Player"
      ];
    })
  ];

  meta = {
    description = "Desktop TIDAL client rewritten in Rust";
    homepage = "https://github.com/Brskt/TidaLuna";
    platforms = [
      "x86_64-linux"
      "aarch64-linux"
    ];
    mainProgram = "tidalunar";
  };
}
