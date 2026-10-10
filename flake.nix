{
  description = "cardano-census - reachability census of the Cardano big ledger peers";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-26.05";

    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };

    iohk-nix = {
      url = "github:input-output-hk/iohk-nix";
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
    iohk-nix,
    naersk,
  }: let
    inherit (nixpkgs) lib;
    systems = ["x86_64-linux" "aarch64-linux"];
    forSystems = lib.genAttrs systems;

    # A path: input carries no git information; the source hash still tells deploys apart.
    gitRev = self.rev or self.dirtyRev or "src-${builtins.substring 0 8 (baseNameOf (toString self.outPath))}";

    crate = {
      pname = "cardano-census";
      version = "0.1.0";
      src = ./.;
    };

    # Everything a system needs, built natively with a static musl target.
    per = system: let
      pkgs = import nixpkgs {inherit system;};
      musl = "${pkgs.stdenv.hostPlatform.qemuArch}-unknown-linux-musl";
      toolchain = with fenix.packages.${system};
        combine [
          stable.rustc
          stable.cargo
          stable.clippy
          targets.${musl}.stable.rust-std
        ];
      naersk-lib = naersk.lib.${system}.override {
        cargo = toolchain;
        rustc = toolchain;
      };
    in rec {
      inherit pkgs toolchain;

      cardano-census = naersk-lib.buildPackage (crate
        // {
          nativeBuildInputs = with pkgs; [
            pkgsStatic.stdenv.cc
          ];

          CARGO_BUILD_TARGET = musl;

          # Only the final link sees the revision, so every commit rebuilds the
          # binary while the dependency build stays cached.
          overrideMain = _: {GIT_REV = gitRev;};
        });

      clippy = naersk-lib.buildPackage (crate // {mode = "clippy";});
      tests = naersk-lib.buildPackage (crate // {mode = "test";});

      devShell = pkgs.mkShell {
        nativeBuildInputs = [
          toolchain
          pkgs.rust-analyzer
        ];
      };
    };

    builds = forSystems per;
  in {
    packages = forSystems (system: rec {
      default = cardano-census;
      inherit (builds.${system}) cardano-census;
    });

    checks = forSystems (system: {
      inherit (builds.${system}) clippy tests;
    });

    devShells = forSystems (system: {
      default = builds.${system}.devShell;
    });

    nixosModules.default = {
      pkgs,
      lib,
      ...
    }: {
      imports = [./nix/module.nix];
      services.cardano-census.package =
        lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.cardano-census;
    };

    hydraJobs = let
      pkgs = builds.x86_64-linux.pkgs;
      jobs =
        forSystems (system: {
          inherit (self.packages.${system}) cardano-census;
          inherit (self.checks.${system}) clippy tests;
          devShell = self.devShells.${system}.default;
        })
        // {
          gitrev = pkgs.writeText "gitrev" gitRev;
        };
    in
      jobs
      // {
        inherit (pkgs.callPackages iohk-nix.utils.ciJobsAggregates {ciJobs = jobs;}) required;
      };
  };
}
