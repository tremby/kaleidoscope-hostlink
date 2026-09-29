{
  description = "kaleidoscope-hostlink";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      supportedSystems = [ "x86_64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
      pkgsFor = nixpkgs.legacyPackages;
    in {
      packages = forAllSystems (system: {
        default = pkgsFor.${system}.callPackage ./default.nix { };
      });

      devShells = forAllSystems (system: {
        default = pkgsFor.${system}.callPackage ./shell.nix { };
      });

      homeManagerModules.default = { pkgs, ... }: {
        home.services.kaleidoscope-hostlink = {
          imports = [
            self.packages.${pkgs.stdenv.hostPlatform.system}.default.passthru.services.default
          ];
        };
      };

      overlays.default = final: prev: {
        kaleidoscope-hostlink =
          self.packages.${prev.stdenv.hostPlatform.system}.default;
      };
    };
}
