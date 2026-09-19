{
  description = "cardano-census - reachability census of the Cardano big ledger peers";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-26.05";

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    naersk = {
      url = "github:nix-community/naersk";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    self,
    nixpkgs,
    fenix,
    naersk,
  }: let
    system = "x86_64-linux";
    pkgs = import nixpkgs {inherit system;};

    toolchain = with fenix.packages.${system};
      combine [
        stable.rustc
        stable.cargo
        stable.clippy
        targets.x86_64-unknown-linux-musl.stable.rust-std
      ];

    naersk-lib = naersk.lib.${system}.override {
      cargo = toolchain;
      rustc = toolchain;
    };
  in {
    packages.${system} = rec {
      default = cardano-census;

      cardano-census = naersk-lib.buildPackage {
        pname = "cardano-census";
        version = "0.1.0";
        src = ./.;

        nativeBuildInputs = with pkgs; [
          pkgsStatic.stdenv.cc
        ];

        CARGO_BUILD_TARGET = "x86_64-unknown-linux-musl";
      };
    };

    nixosModules.default = {
      pkgs,
      lib,
      ...
    }: {
      imports = [./nix/module.nix];
      services.cardano-census.package =
        lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.cardano-census;
    };

    devShells.${system}.default = pkgs.mkShell {
      nativeBuildInputs = [
        toolchain
        pkgs.rust-analyzer
      ];
    };
  };
}
