{
  description = "eframe devShell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
      in with pkgs; {
        devShells.default = mkShell rec {
          buildInputs = [
            # Rust
            (pkgs.rust-bin.beta.latest.default.override {
              extensions = [ "rust-analyzer" ];
            })
            trunk

            pkg-config

            libxkbcommon
            libGL
            fontconfig

            wayland
          ];

          LD_LIBRARY_PATH = "${lib.makeLibraryPath buildInputs}";
        };
      });
}
