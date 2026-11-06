# Precept build, test and verification recipes. Run `just <recipe>` from the
# repo root (inside `devenv shell`, which pins Rust and the Stellar CLI).
# Variables in UPPER_CASE can be overridden from the environment or as
# `just NAME=value <recipe>`.
# Never load a .env: a parent workspace's may hold credentials.
set dotenv-load := false

root := justfile_directory()

# Build factory → backstop → pool wasm, then optimize them.
default: build

test: build
    cargo test --all --tests

build:
    cargo rustc --manifest-path=pool-factory/Cargo.toml --crate-type=cdylib --target=wasm32v1-none --release
    cargo rustc --manifest-path=backstop/Cargo.toml --crate-type=cdylib --target=wasm32v1-none --release
    cargo rustc --manifest-path=pool/Cargo.toml --crate-type=cdylib --target=wasm32v1-none --release
    mkdir -p target/wasm32v1-none/optimized
    stellar contract optimize \
        --wasm target/wasm32v1-none/release/pool_factory.wasm \
        --wasm-out target/wasm32v1-none/optimized/pool_factory.wasm
    stellar contract optimize \
        --wasm target/wasm32v1-none/release/backstop.wasm \
        --wasm-out target/wasm32v1-none/optimized/backstop.wasm
    stellar contract optimize \
        --wasm target/wasm32v1-none/release/pool.wasm \
        --wasm-out target/wasm32v1-none/optimized/pool.wasm
    cd target/wasm32v1-none/optimized/ && ls -l *.wasm

# Explicit base-versus-fork differential (ADR 0008 step 8 / ADR 0011 step 5).
# Not part of `just test`: builds the pinned stock baseline from git, then runs the
# #[ignore] runner against both optimized artifact pairs. Evidence lands in
# target/adr8-differential.
ADR8_BASE_COMMIT := env_var_or_default("ADR8_BASE_COMMIT", "6cf6dd1ca712ea3cf17a767fe49b7da01f80b909")
adr8_base_dir := root / "target/adr8-base"
# The pinned stock baseline still builds with its own (pre-upgrade) toolchain and
# its own upstream Makefile, so its artifacts stay under wasm32-unknown-unknown and
# it is built with `make`. The fork builds for wasm32v1-none (see `build` above).
adr8_base_opt := adr8_base_dir / "target/wasm32-unknown-unknown/optimized"
adr8_fork_opt := root / "target/wasm32v1-none/optimized"

differential: build
    rm -rf "{{adr8_base_dir}}" && mkdir -p "{{adr8_base_dir}}"
    git archive {{ADR8_BASE_COMMIT}} | tar -x -C "{{adr8_base_dir}}"
    make -C "{{adr8_base_dir}}" build
    ADR8_BASE_POOL_WASM="{{adr8_base_opt}}/pool.wasm" \
    ADR8_BASE_BACKSTOP_WASM="{{adr8_base_opt}}/backstop.wasm" \
    ADR8_FORK_POOL_WASM="{{adr8_fork_opt}}/pool.wasm" \
    ADR8_FORK_BACKSTOP_WASM="{{adr8_fork_opt}}/backstop.wasm" \
    ADR8_DIFF_OUTPUT_DIR="{{root}}/target/adr8-differential" \
    cargo test -p test-suites --test adr8_differential -- --ignored --nocapture

KANI_RELEASE_REPO := env_var_or_default("KANI_RELEASE_REPO", "Templar-Protocol/blend-contracts-v2")
KANI_RELEASE_COMMIT := env_var_or_default("KANI_RELEASE_COMMIT", "HEAD")
KANI_RELEASE_PACKAGE_DIR := env_var_or_default("KANI_RELEASE_PACKAGE_DIR", "target/kani-package")
KANI_RELEASE_RUN_DIR := env_var_or_default("KANI_RELEASE_RUN_DIR", "target/kani-run")
KANI_RELEASE_OUTPUT_DIR := env_var_or_default("KANI_RELEASE_OUTPUT_DIR", "target/kani-releases")
KANI_RELEASE_DRY_RUN := env_var_or_default("KANI_RELEASE_DRY_RUN", "")

kani-release:
    python3 -m unittest scripts/test_kani_release.py
    python3 scripts/kani_release.py \
        --repo "{{KANI_RELEASE_REPO}}" \
        --commit "{{KANI_RELEASE_COMMIT}}" \
        --package-dir "{{KANI_RELEASE_PACKAGE_DIR}}" \
        --run-dir "{{KANI_RELEASE_RUN_DIR}}" \
        --output-dir "{{KANI_RELEASE_OUTPUT_DIR}}" {{ if KANI_RELEASE_DRY_RUN != "" { "--dry-run" } else { "" } }}

verify-local:
    python3 -m unittest scripts/test_verify_local.py scripts/test_kani_release.py \
        scripts/test_verification_tools.py scripts/test_proof_ci.py
    python3 scripts/verify_local.py

verify-local-codegen:
    python3 scripts/verify_local.py --codegen

# Emits JSON Lines on stdout (just echoes recipe lines to stderr).
fuzz-replay:
    @cd test-suites/fuzz && cargo run --quiet --locked --release --bin replay_pool_general -- regression-seeds

fuzz-test:
    cd test-suites/fuzz && cargo test --locked --lib

fuzz-lint:
    cargo clippy --locked --manifest-path test-suites/fuzz/Cargo.toml --all-targets -- -D warnings

fuzz-build:
    cd test-suites/fuzz && cargo fuzz build fuzz_pool_general

fmt:
    cargo fmt --all
    cargo fmt --manifest-path test-suites/fuzz/Cargo.toml --all

clean:
    cargo clean

generate-js:
    stellar contract bindings typescript --overwrite \
        --contract-id CBWH54OKUK6U2J2A4J2REJEYB625NEFCHISWXLOPR2D2D6FTN63TJTWN \
        --wasm ./target/wasm32v1-none/optimized/backstop.wasm --output-dir ./js/js-backstop/ \
        --rpc-url http://localhost:8000 --network-passphrase "Standalone Network ; February 2017" --network Standalone
    stellar contract bindings typescript --overwrite \
        --contract-id CBWH54OKUK6U2J2A4J2REJEYB625NEFCHISWXLOPR2D2D6FTN63TJTWN \
        --wasm ./target/wasm32v1-none/optimized/pool_factory.wasm --output-dir ./js/js-pool-factory/ \
        --rpc-url http://localhost:8000 --network-passphrase "Standalone Network ; February 2017" --network Standalone
    stellar contract bindings typescript --overwrite \
        --contract-id CBWH54OKUK6U2J2A4J2REJEYB625NEFCHISWXLOPR2D2D6FTN63TJTWN \
        --wasm ./target/wasm32v1-none/optimized/pool.wasm --output-dir ./js/js-pool/ \
        --rpc-url http://localhost:8000 --network-passphrase "Standalone Network ; February 2017" --network Standalone
