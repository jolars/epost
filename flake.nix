{
  description = "epost — linux maildir email reader/composer";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems =
        f:
        nixpkgs.lib.genAttrs systems (
          system:
          f {
            inherit system;
            pkgs = import nixpkgs { inherit system; };
          }
        );
      cargoToml = builtins.fromTOML (builtins.readFile ./Cargo.toml);
    in
    {
      packages = forAllSystems (
        { pkgs, ... }:
        {
          default = pkgs.rustPlatform.buildRustPackage {
            pname = cargoToml.package.name;
            version = cargoToml.package.version;

            src = self;

            cargoLock = {
              lockFile = ./Cargo.lock;
            };

            # Tests touch stdio / tmp maildirs and are exercised in devenv;
            # skip them at build time to keep nix builds hermetic.
            doCheck = false;

            meta = {
              description = "Linux maildir email reader/composer (TUI)";
              mainProgram = "epost";
              platforms = systems;
            };
          };
        }
      );
    };
}
