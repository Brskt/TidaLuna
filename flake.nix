{
  description = "TidaLunar - A TIDAL client";

  inputs.nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";

  # Unstable dropped x86_64-darwin in 26.11. This branch is the last to carry it
  # and stops being built at the end of 2026: drop the platform by then.
  inputs.nixpkgs-x86-darwin.url = "github:nixos/nixpkgs/nixpkgs-26.05-darwin";

  outputs =
    {
      self,
      nixpkgs,
      nixpkgs-x86-darwin,
    }:
    let
      systems = [
        "x86_64-linux"
        "x86_64-darwin"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      pkgsFor =
        system:
        if system == "x86_64-darwin" then
          nixpkgs-x86-darwin.legacyPackages.${system}
        else
          nixpkgs.legacyPackages.${system};

      # The release tarball that package.nix wraps ships Linux binaries only.
      packagedSystem = system: nixpkgs.lib.hasSuffix "-linux" system;
    in
    {
      packages = forAllSystems (
        system:
        nixpkgs.lib.optionalAttrs (packagedSystem system) {
          default = (pkgsFor system).callPackage ./nix/package.nix { };
        }
      );

      apps = forAllSystems (
        system:
        nixpkgs.lib.optionalAttrs (packagedSystem system) {
          default = {
            type = "app";
            program = "${self.packages.${system}.default}/bin/tidalunar";
          };
        }
      );

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          default = pkgs.callPackage ./nix/devshell.nix { };
        }
      );

      # The tree wrapper, not bare nixfmt: `nix fmt` passes no arguments, and
      # nixfmt then fails on an empty stdin.
      formatter = forAllSystems (system: (pkgsFor system).nixfmt-tree);
    };
}
