{
  description = "skein — wallpaper-driven color scheme generator and theme daemon";

  inputs.nixpkgs.url = "nixpkgs";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAll = nixpkgs.lib.genAttrs systems;
    in {
      packages = forAll (system: {
        default = nixpkgs.legacyPackages.${system}.rustPlatform.buildRustPackage {
          pname = "skein";
          version = "0.1.0";
          src = ./.;
          cargoLock.lockFile = ./Cargo.lock;
          meta = with nixpkgs.lib; {
            description = "Wallpaper-driven color scheme generator and theme daemon";
            license = with licenses; [ mit asl20 ];
            mainProgram = "skeind";
          };
        };
      });

      homeModules.default = { config, lib, pkgs, ... }: {
        imports = [ ./nix/home-manager.nix ];
        config.services.skein.package = lib.mkDefault self.packages.${pkgs.system}.default;
      };

      devShells = forAll (system: {
        default = nixpkgs.legacyPackages.${system}.mkShell {
          packages = with nixpkgs.legacyPackages.${system}; [
            cargo rustc gcc clippy rustfmt rust-analyzer
          ];
        };
      });
    };
}
