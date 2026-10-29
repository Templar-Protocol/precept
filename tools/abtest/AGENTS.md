# Repository Guidelines

Guide for AI assistants working on `tools/abtest` (`precept-abtest`), a subdirectory of
`Templar-Protocol/precept` with its own Cargo workspace. The ADRs it checks (0008, 0009,
0011) are Templar's design records; the invariants they name are spelled out in this crate
(`src/report.rs`, `src/observe.rs`) and in `test-suites/`.

## Project Overview

Rust CLI + operator TUI that deploys two Blend v2 pools on Stellar **testnet** and A/B-drives
identical step sequences against both, in lockstep, through the external `stellar` CLI
(`README.md:3-8`). Every divergence is classified Match / Expected (per pair profile) /
Violation; per-step evidence is retained as JSON; `report.md` carries a PASS/FAIL verdict and
an ADR-keyed invariant ledger (ADR 0008/0009/0011 clauses). The binary name is
`precept-abtest` (`Cargo.toml:1-6`); the crate is its own Cargo workspace.

Pairs: pinned stock Blend v2 (commit 6cf6dd1) vs the ADR-0008 security fork
(`pairs/stock-vs-fork.toml`), or two fork pools with different reserve configs
(`pairs/fork-vs-fork.toml`).

## Architecture & Data Flow

```
pairs/*.toml --deploy--> <out>/manifest.toml --run--> evidence/<scenario>/<NNN>-<label>/*.json --report--> <out>/report.md
PairConfig               PairManifest + state.json            a.json / b.json / diff.json                  verdict + ADR ledger
```

- **Chain seam — `src/stellar.rs`.** The crate has no RPC/XDR/signing code: an `Executor`
  trait (`src/stellar.rs:76-78`) wraps the external `stellar` binary; `RealExecutor` spawns
  it with `STELLAR_CONTRACT_ID` removed and `XDG_CONFIG_HOME` + cwd pinned to the run dir so
  the CLI's env/config auto-load cannot hijack calls (`src/stellar.rs:80-88`). Output parsing
  is central: JSON stdout, stderr `📅 ... Event:` lines, `Error(Contract, #code)` errors, and
  `contract read --output xdr` CSV rows. A retry layer (5 attempts) retries transient network
  errors but **never blindly replays a mutating call after an ambiguous error** — a tx that
  may have landed must not double-apply; `ResourceLimitExceeded` is the documented exception
  (`src/stellar.rs:42-45`). That layer is the only retry that exists: no CLI or TUI control
  re-sends a canonical step.
- **Config/manifests — `src/manifest.rs`.** `PairConfig` reads `pairs/*.toml` (profile,
  wasm paths with `~/` expansion, optional per-token `ReserveOverride`s); `PairManifest`
  records what was deployed; `RunState` (`state.json`) persists completed steps for resumable
  runs; `RunLock` is a pid-based lock making a run dir owned by one live process
  (`src/manifest.rs:347-353`); a second process opens the run read-only.
- **Deploy — `src/deploy.rs`.** Mirrors the upstream test fixture
  (`blend-contracts-v2/test-suites/src/{test_fixture.rs,setup.rs}`): friendbot-funded
  identities (`Who::Admin/Frodo/Sam/User`, `src/deploy.rs:31-38`), shared wasm uploads, and
  per-side deploys keyed by a deterministic salt `salt(nonce, role, side)`
  (`src/deploy.rs:23-29`).
- **Scenarios — `src/scenario.rs`.** Data-only: `Step` variants, `Expect` outcomes, and 9
  `TrapCall` probes with labels + the error code the fork must return
  (`src/scenario.rs:57-104`). Ported from the upstream differential suite with testnet
  substitutions (waits + oracle reprices instead of ledger jumps). Scenarios are **hardcoded
  in code**, never in TOML. `freeze` is always last (status 4 is absorbing). Selection is
  family-scoped by one authority — `FAMILIES`, `family_of`, `loss_prerequisites_ok`,
  `SELECTION_GUIDANCE` (`src/scenario.rs:316-370`): one isolated family per invocation, on
  its own fresh deployment, and no `all`.
- **Run loop — `src/run.rs`.** `Runner` executes each labeled step on both sides in lockstep
  (`step_once`): execute → snapshot both sides → per-side `invariants()` (plus
  `no_state_change` for traps) → `compare_step` classification → persist evidence → mark
  `state.json`. An injected `Ev`/`Act` hook gates on StepStarted/StepDone/Halt/Waiting;
  headless runs print byte-identical legacy stderr lines (`legacy_line`), `run --json` emits
  one compact JSON object per event. There is **no replay control**: completed evidence is
  never overwritten, a `pending/` journal with an unsettled chain outcome makes
  `Runner::new` refuse the run dir, and the dependency barrier is recomputed from retained
  evidence on every resume. Operator-initiated steps go through the same path and are
  recorded under `evidence/operator/`, excluded from the verdict, disclosed in `report.md`.
- **Observation — `src/observe.rs`.** `snapshot()` builds a typed `Snapshot` (positions,
  reserves, config, backstop, reward zone, balances, auctions, raw rows) via parallel
  read-only view jobs (`src/observe.rs:70-71`); `invariants()` ports the upstream
  `adr8_properties.rs` invariant set and returns human-readable failures
  (`src/observe.rs:331`).
- **Comparison — `src/compare.rs`.** `Class {Match, Expected, Violation}`
  (`src/compare.rs:15-20`); fork-only/stock-only event names decide which event-set
  divergences are expected (`src/compare.rs:32-38`); numeric leaves compare at a
  profile-dependent relative tolerance — 1e-5 stock-vs-fork, 1e-2 fork-vs-fork
  (`src/compare.rs:582-587`); profile-specific accounting assumptions come only from the
  typed `invariant_policy` (`src/run.rs:131-139`) and `InvariantPolicy`
  (`src/observe.rs:569-572`). The old string-matched waiver list is gone: an invariant
  message can never excuse a divergence.
- **Report — `src/report.rs`.** Reloads evidence, re-classifies it under the *current*
  manifest profile, writes `<out>/report.md`: per-scenario tables, `## ADR invariant ledger`
  (one row per ADR clause, judged on the fork side; both sides under fork-vs-fork), and a
  `## Verdict` table — PASS iff violations == 0 and invariant failures == 0. Exit codes:
  0 pass, 2 fail verdict, 1 tool failure (`verdict_code`, `src/report.rs:117-124`).
- **TUI — `src/tui/`.** `mod.rs` spawns a worker thread that drives the same `Runner` over
  channels (`Ev` down / `Act` up); every CLI call is mirrored into the stream panel via
  `TapExecutor`. `app.rs` holds state with 8 tabs (`Tab::ALL`: Invariants, Diff, Snapshot,
  Stream, Flow, Rates, Traps, Health); `render.rs` draws; `palette.rs` runs ad-hoc operator
  steps through the production step path. Driving needs an explicit `--scenario` family;
  `--watch` never takes the run lock. Controls are pause/step/skip/abort plus the
  disclosed `R` operator action — nothing here can replay canonical evidence.
- **Run-level analysis — `src/timeline.rs`.** Pure functions over evidence already on disk
  (no new chain reads): ADR 0008 §8 flow attribution (every `b_supply`/`d_supply` move must
  be explained by an emitted event), rate series, trap rows, health gauges, B-side orphan
  custody tracking (`src/timeline.rs:1-9`). The orphan history reads only `b.snapshot` and
  `m.b`, so it is the fork-side history, not a both-fork completeness claim.

## Key Directories

- `src/` — one module per concern: `main` (clap CLI + run discovery), `stellar` (chain seam),
  `manifest` (config/state/lock), `release` (contracts-release artifact resolver), `deploy`,
  `scenario` (data only), `run` (lockstep runner), `observe` (snapshots + invariants),
  `compare` (classification), `report` (evidence + md), `timeline` (run-level ADR §8 analysis).
- `src/tui/` — operator terminal: `mod` (event loop), `app` (state), `render` (draw),
  `palette` (ad-hoc steps).
- `pairs/` — pair profiles (TOML in; committed).
- `runs/` — gitignored run state: `manifest.toml`, `state.json`, `run.lock`, `pending/`
  (per-side mutation journal), `.stellar/` (friendbot-funded identities, reused), `.xdg/`,
  `evidence/<scenario>/<NNN>-<label>/`, `report.md` (`README.md:63-67`). `--out` is optional
  everywhere and defaults to the newest run under `./runs` (`README.md:90-92`).

## Development Commands

Build **inside the repo devenv shell** (pins Rust 1.98.0 to match the contracts):

```sh
devenv shell -- bash -c 'cd tools/abtest && cargo build --release'   # from repo root
cargo test                                                            # offline, no network/terminal
```

Full workflow (`README.md:40-47`, run from `tools/abtest/`):

```sh
export STELLAR_BIN="$PRECEPT_STELLAR_BIN"   # devenv exports its pinned CLI
cargo run -- --out runs/$(date +%s) deploy --pair pairs/stock-vs-fork.toml
cargo run -- --out runs/<ts> run --scenario lending,interest,statuses,backstop,stress,freeze
cargo run -- --out runs/<ts> run --scenario strict_oracle
cargo run -- --out runs/<ts> run --scenario liquidation,stale_auction,direct_default
cargo run -- --out runs/<ts> report                        # exit code = verdict (0 pass / 2 fail)
```

One family per run dir, on a deployment made for it: `deploy` refuses a dir that already
holds one, and `persist_requested` refuses a second family before saving any scope. There
is no `all`.

- `precept-abtest list` — runs under `./runs`, how far each got, who owns the lock.
- `precept-abtest tui --scenario <family>` (drive; takes the lock) / `tui --watch` (read-only follow/browse).
- `run --from-step <label>` only moves where the scan starts; it cannot re-drive a label whose evidence exists.
- `run --json` — one JSON object per event on stdout, for automation.
- Negative control: copy only the public manifest, state, and evidence into offline scratch;
  change the copied profile to `fork-vs-fork` and report that copy. Trap and bad-debt steps
  must fail. Preserve the original campaign and never copy its identity directories.

## Code Conventions & Common Patterns

- **Errors: anyhow-only.** No thiserror/custom error enums; `Context`/`ensure!`/`bail!`
  throughout; `main` prints `error: {e:#}` and exits 1 (`src/main.rs:82-90`). `ErrorKind`
  (`src/stellar.rs`) classifies *contract* errors, not the error path. Contract error codes
  get names and plain-language explanations in `error_name`/`error_help`
  (`src/stellar.rs:472-562`) — a bare number tells nobody anything.
- **CLI: clap 4 derive** with env fallbacks (`STELLAR_BIN`, `STELLAR_RPC_URL`,
  `STELLAR_NETWORK_PASSPHRASE`) and testnet-safe defaults (`src/main.rs:26-38`).
- **Scenario changes are label changes.** A step's label is its identity: `state.json`
  resume, diff classification, report ledger rows, and trap probes all match labels exactly.
  When renaming/adding labels, grep every consumer (`compare.rs`, `report.rs` `find()`,
  `timeline.rs`, tests) — the README's scenario list has drifted before (see gotchas).
- **Numeric comparisons** always go through the profile `tolerance()`; never hardcode an
  epsilon. Delta-dependent steps (auction fills) compare structure only, because inclusion
  ledgers are not controllable on testnet.
- **Determinism:** deploy salts and the `stress` scenario (seeded LCG, seed + step count
  fixed in `scenario.rs`) are deterministic so failures reproduce exactly.
- **Test doubles:** `FakeExecutor` in `#[cfg(test)] pub mod fake` (`src/stellar.rs:718+`)
  scripts CLI outputs and records argv; TUI tests render through ratatui `TestBackend`.
  Shared helpers live in `pub mod tests` blocks.
- **Test names** are behavior sentences in snake_case, with doc comments on subtle ones.
- **Provenance comments** name the upstream file a module ports (`blend-contracts-v2/
  test-suites/src/...`) — keep them when porting more.
- **Commits:** `abtest: <imperative summary>` (see `git log`).

### Gotchas

- **Regenerate `Cargo.lock` only deliberately.** `clap` is held at `=4.5.20` and the
  lockfile pins `instability`, `darling`, `unicode-segmentation` to fixed releases chosen
  for older MSRVs; a regenerated lock rewrites that resolution and silently changes what
  the devenv build sees (`README.md:18-22`).
- **Label drift is a live defect class.** A step's label is matched by string in
  `compare.rs` (`probe`, `probe_state_allowed`, `designed_stock_probe`), `report.rs` ledger
  rows, `timeline.rs::expectations`, and tests — the old `default_orphan_*` prefix drifted
  from the real `direct_default_*` labels and silently dead-ended the r7/r4 rows (fixed
  2026-09-17 together with the README scenario list). When renaming/adding labels, grep
  every consumer; `timeline` now also surfaces unknown labels in the status line.
- **`STELLAR_CONTRACT_ID` is hostile env.** The shell may export it; `RealExecutor` strips
  it because it hijacks `--id`. Any new spawn path must do the same.
- **Testnet reset invalidates a run.** `Runner::preflight` re-reads `get_config` from both
  pools and fails with "side … pool unreachable (testnet reset?)" — don't fight it; redeploy.
- **Scenario order matters:** `freeze` is absorbing (status 4); never reorder it before
  other scenarios. Traps run after every scenario.

## Important Files

- `README.md` — canonical recipes; this doc defers to it for exact invocations.
- `Cargo.toml` / `Cargo.lock` — deps + the 1.98.0 lock discipline above.
- `src/stellar.rs` — every chain interaction, parser, retry/replay law. Touch this before
  inventing a new way to call the chain.
- `src/scenario.rs` / `src/run.rs` — what gets tested and how it is driven.
- `src/compare.rs` / `src/report.rs` — what counts as Expected vs Violation; the ADR ledger.
- `src/observe.rs` — snapshots + the invariant set (upstream port).
- `src/timeline.rs` — ADR 0008 §8 flow attribution over retained evidence.
- `pairs/*.toml` — wasm inputs (paths must exist; `PairConfig::load` asserts).

## Runtime/Tooling Preferences

- **Rust 1.98.0 pinned** (`rust-toolchain.toml`, matches `Cargo.toml` `rust-version`) to match
  the vendored contracts; build through the parent repo's devenv shell.
- **CLI versions are three separate pins**: the pair gate (`MIN_CLI_MAJOR`,
  `src/stellar.rs:13`) requires **>= 23**; the market explorer gate requires
  **>= 26.1.0** and takes the exact binary devenv exports as
  `PRECEPT_STELLAR_BIN` (raw cargo invocations: `STELLAR_BIN="$PRECEPT_STELLAR_BIN"`).
  The `stellar` on `PATH` in the devenv shell satisfies both gates.
- **Wasm inputs** come from this repository: fork-side from `make build` under
  `../../target/wasm32v1-none/optimized`, the stock baseline from `make differential`
  under `../../target/adr8-base/target/wasm32-unknown-unknown/optimized`, plus registry mock
  token/oracle/emitter wasm; `pairs/*.toml` holds the paths
  (`README.md:30-35`). The market's proxy stack is the exception:
  `markets/protected-testnet.toml` pins `proxy-runtime`/`proxy-governance` in
  release form, and `src/release.rs` resolves them from the contracts
  catalog (`contract/artifacts/releases/<key>@<version>.tsv` on `dev`) —
  verified length+SHA-256 against the row, cached under
  `$XDG_CACHE_HOME/precept-abtest/artifacts/`, frozen exact-version into the
  run's `deployment-plan.toml`; never a local build, never re-resolved on
  resume.
- **Network.** Paired runs and lab market runs stay on testnet: identities are freshly
  generated, friendbot-funded keys stored under the run's `.stellar/`; network defaults:
  `https://soroban-testnet.stellar.org` + the Test SDF passphrase. `market deploy --market
  <lab.toml>` runs the lab market graph, unchanged. `market deploy --config <pool.json>
  --network <testnet|mainnet>` deploys a real market from a precept pool config, so the
  deployer has a mainnet path: `mainnet` is a network a config may name and a value the
  operator passes on the command line. `--network` belongs to `--config` only, and `--market`
  and `--config` are exclusive. Both paths share the tool's `--out` run directory and the
  same progress output. No mainnet send has happened yet, and the testnet rehearsal is
  unfinished. README carries the exact invocation surface.
- **Real market from a precept pool config.** `precept/registry/configs/*.json` shape, with
  an `oracle` block giving the per-asset oracle proxy configuration; `pool.oracle` is an
  optional existing-adapter override. Same journaled, resumable, gated executor as the lab
  graph. A real run records in its manifest: the network label, every contract id, the
  per-asset oracle-proxy configuration, the synthetic-leg issuers and their classic asset
  codes, and the launch artifact pins used with where each came from (an operator-declared
  `live` pin, or a local build whose bytes hashed to that digest). Launches are per-pool
  isolated: each pool has its own backing pool, price adapter, pool contract, synthetic-leg
  issuers and assets. Runtime and governance are the only shared contracts; a config naming
  them reuses them and never redeploys, and a first launch on a network without them
  uploads and deploys them.

## Testing & QA

- `cargo test` is **fully offline by design**: scripted `FakeExecutor`, a fake `stellar`
  shell script for the binary-level tests in `tests/`, and ratatui `TestBackend` — no
  terminal, no network, no chain. The test runner reports the current count.
- Coverage to keep when changing code: the transport retry/no-blind-replay law and the
  pending-custody/no-replay law (`stellar.rs`, `run.rs`), step lifecycle + resumption
  (`run.rs`, `manifest.rs` lock tests), classification boundaries (`compare.rs`),
  ledger/TUI rendering at multiple viewport sizes (`render.rs`
  `renders_at_every_size_without_panicking`), and determinism (`scenario.rs`).
- CI: `.github/workflows/abtest.yml` runs clippy `-D warnings` and the tests. The crate is
  not rustfmt-clean; format only the files a change touches (`rustfmt --edition 2021 <file>`).
  Contract-level verification (`make test`, Kani, testnet smoke) lives in the repository
  README — abtest itself *is* the differential rung.
- The runtime QA artifact is `report.md`: verdict + ADR invariant ledger. The TUI's
  Invariants tab recomputes the same ledger rows live, so a failing run is visible mid-run.
