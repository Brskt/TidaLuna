{
  description = "TidaLunar - A TIDAL client";

  inputs.nixpkgs.url = "github:nixos/nixpkgs/nixpkgs-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      pkgsFor = system: nixpkgs.legacyPackages.${system};

      # The release tarball that package.nix wraps ships Linux binaries only.
      packagedSystem = system: nixpkgs.lib.hasSuffix "-linux" system;
    in
    {
      packages = forAllSystems (system:
        nixpkgs.lib.optionalAttrs (packagedSystem system) {
          default = (pkgsFor system).callPackage ./nix/package.nix { };
        });

      apps = forAllSystems (system:
        nixpkgs.lib.optionalAttrs (packagedSystem system) {
          default = {
            type = "app";
            program = "${self.packages.${system}.default}/bin/tidalunar";
          };
        });

      devShells = forAllSystems (
        system:
        let
          pkgs = pkgsFor system;
        in
        {
          default = pkgs.callPackage ./nix/devshell.nix { };
        }
      );

      formatter = forAllSystems (system: (pkgsFor system).nixfmt);
    };
}
