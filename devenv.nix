{ pkgs, lib, inputs, ... }:

let
  # The repo's pinned toolchain, also used to build the Stellar CLI below:
  # stellar-cli declares a rust-version newer than nixpkgs' stable rustc, so
  # it is built with the toolchain the repo itself pins.
  rustPkgs = import inputs.nixpkgs {
    inherit (pkgs.stdenv.hostPlatform) system;
    overlays = [ (import inputs.rust-overlay) ];
  };
  toolchain = rustPkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
  rustPlatform = pkgs.makeRustPlatform {
    cargo = toolchain;
    rustc = toolchain;
  };

  # `make build` and `make differential` need `stellar contract optimize`, which
  # refuses to run unless the CLI was built with `additional-libs` (it is what
  # pulls in wasm-opt). stellar-cli is not packaged in nixpkgs; build the pinned
  # release from crates.io.
  stellar-cli = rustPlatform.buildRustPackage rec {
    pname = "stellar-cli";
    version = "28.1.0";

    src = pkgs.fetchCrate {
      inherit pname version;
      hash = "sha256-8RT4IqnlM7h3WpchhB+eVJ6vTqHd4e+nKCbt/ffTQPY=";
    };

    cargoHash = "sha256-hVRxXU+ChsabKG1HqDcN1yeVZxkA7ylbZxJY2uEPCqw=";

    buildFeatures = [ "additional-libs" ];

    # additional-libs brings in binaryen (cmake), hidapi (libudev, from systemd)
    # and the secret-service keyring backend (dbus). HTTP/TLS is rustls, so no
    # openssl.
    nativeBuildInputs = with pkgs; [ pkg-config cmake ];
    buildInputs = with pkgs; [ dbus systemd ];

    # Upstream tests want a live network.
    doCheck = false;

    meta = {
      description = "Stellar CLI pinned for reproducible Soroban Wasm optimization";
      mainProgram = "stellar";
    };
  };
in
{
  packages = [ stellar-cli ];

  # tools/abtest's justfile and market commands take the CLI by exact path.
  env.PRECEPT_STELLAR_BIN = lib.getExe stellar-cli;

  languages.rust = {
    enable = true;
    toolchainFile = ./rust-toolchain.toml;
  };
}
