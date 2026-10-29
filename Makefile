default: build

test: build
	cargo test --all --tests

build:
	cargo rustc --manifest-path=pool-factory/Cargo.toml --crate-type=cdylib --target=wasm32v1-none --release
	cargo rustc --manifest-path=backstop/Cargo.toml --crate-type=cdylib --target=wasm32v1-none --release
	cargo rustc --manifest-path=pool/Cargo.toml --crate-type=cdylib --target=wasm32v1-none --release
	cargo rustc --manifest-path=oracle-tripper/Cargo.toml --crate-type=cdylib --target=wasm32v1-none --release
	
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
	stellar contract optimize \
		--wasm target/wasm32v1-none/release/oracle_tripper.wasm \
		--wasm-out target/wasm32v1-none/optimized/oracle_tripper.wasm
	cd target/wasm32v1-none/optimized/ && \
		for i in *.wasm ; do \
			ls -l "$$i"; \
		done

# Explicit base-versus-fork differential (ADR 0008 step 8 / ADR 0011 step 5).
# Not part of `make test`: builds the pinned stock baseline from git, then runs the
# #[ignore] runner against both optimized artifact pairs. Evidence lands in
# target/adr8-differential.
ADR8_BASE_COMMIT ?= 6cf6dd1ca712ea3cf17a767fe49b7da01f80b909
ADR8_BASE_DIR = target/adr8-base
# The pinned stock baseline still builds with its own (pre-upgrade) toolchain and
# Makefile, so its artifacts stay under wasm32-unknown-unknown. The fork builds
# for wasm32v1-none (see the `build` target above).
ADR8_BASE_OPT = target/wasm32-unknown-unknown/optimized
ADR8_FORK_OPT = target/wasm32v1-none/optimized

differential: build
	rm -rf $(ADR8_BASE_DIR) && mkdir -p $(ADR8_BASE_DIR)
	git archive $(ADR8_BASE_COMMIT) | tar -x -C $(ADR8_BASE_DIR)
	$(MAKE) -C $(ADR8_BASE_DIR) build
	ADR8_BASE_POOL_WASM=$(abspath $(ADR8_BASE_DIR)/$(ADR8_BASE_OPT)/pool.wasm) \
	ADR8_BASE_BACKSTOP_WASM=$(abspath $(ADR8_BASE_DIR)/$(ADR8_BASE_OPT)/backstop.wasm) \
	ADR8_FORK_POOL_WASM=$(abspath $(ADR8_FORK_OPT)/pool.wasm) \
	ADR8_FORK_BACKSTOP_WASM=$(abspath $(ADR8_FORK_OPT)/backstop.wasm) \
	ADR8_DIFF_OUTPUT_DIR=$(abspath target/adr8-differential) \
	cargo test -p test-suites --test adr8_differential -- --ignored --nocapture

KANI_RELEASE_REPO ?= Templar-Protocol/blend-contracts-v2
KANI_RELEASE_COMMIT ?= HEAD
KANI_RELEASE_PACKAGE_DIR ?= target/kani-package
KANI_RELEASE_RUN_DIR ?= target/kani-run
KANI_RELEASE_OUTPUT_DIR ?= target/kani-releases

kani-release:
	python3 -m unittest scripts/test_kani_release.py
	python3 scripts/kani_release.py \
		--repo "$(KANI_RELEASE_REPO)" \
		--commit "$(KANI_RELEASE_COMMIT)" \
		--package-dir "$(KANI_RELEASE_PACKAGE_DIR)" \
		--run-dir "$(KANI_RELEASE_RUN_DIR)" \
		--output-dir "$(KANI_RELEASE_OUTPUT_DIR)" $(if $(KANI_RELEASE_DRY_RUN),--dry-run)

.PHONY: verify-local verify-local-codegen fuzz-replay fuzz-test fuzz-lint fuzz-build
verify-local:
	python3 -m unittest scripts/test_verify_local.py scripts/test_kani_release.py \
		scripts/test_verification_tools.py scripts/test_proof_ci.py
	python3 scripts/verify_local.py

verify-local-codegen:
	python3 scripts/verify_local.py --codegen

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
