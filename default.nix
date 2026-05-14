{ pkgs ? import <nixpkgs> {} }:

pkgs.rustPlatform.buildRustPackage {
  pname = "spin";
  version = "0.1.0";
  src = ./.;
  cargoLock.lockFile = ./Cargo.lock;

  meta = {
    description = "Simple Package Installer for Nix";
    mainProgram = "spin";
  };
}
