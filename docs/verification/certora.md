# Certora Sunbeam runbook

[VERIFICATION.md](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/VERIFICATION.md) is the authoritative report for verification
status, claim scopes, assumptions, limitations and evidence provenance. This guide
covers tooling, configuration and reading or preserving Certora artifacts; it
does not extend any claim in that report.

## Source and configuration map

All paths below are relative to the repository root.

| Location | Purpose |
| --- | --- |
| `pool/Cargo.toml` | Optional `certora` feature and CVLR dependencies |
| `Cargo.toml`, `Cargo.lock` | SDK/CVLR dependency pins and release profile |
| `pool/src/spec/mod.rs` | Rule modules and feature-gated summary macros |
| `pool/src/spec/{user_rules,pool_status_rules,health_rules}.rs` | Ported auditor rules |
| `pool/src/spec/{oracle_rules,cap_rules}.rs` | Fork-specific oracle and cap rules |
| `pool/src/spec/{model,oracle,token}.rs`, `pool/src/spec/summaries/` | Ghost state, external interfaces and summaries |
| `pool/confs/` | Selected rules, loop bounds, prover settings and job budgets |
| `pool/certora_build.py`, `pool/justfile` | Certora build callback and instrumented Wasm recipe |
| `verification/certora/` | Archived per-rule `output.json` files |
| `audits/` | Original June 2025 Certora draft report |

The `certora` feature is off in normal builds. Instrumented verification Wasm
uses summaries and feature-gated paths; it is not the normal optimized production
artifact. Consult the central report before transferring a verdict between them.

## Local prerequisites and build

Use the repository Rust/Wasm toolchain and Stellar CLI described in the
[shared preparation instructions](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/VERIFICATION.md#reproducing-this).

Certora has a separate, existing **pool-local** Justfile. Install `just` before
using its build callback. From `pool/`, `certora_build.py` invokes `just build`:
`build-deps` builds the SDK and the factory/backstop Wasm dependencies, then the
pool recipe builds with `--features certora`, `--release`, `wasm32v1-none` and
`RUSTFLAGS="-C strip=none"`. Its reported executable is
`target/wasm32v1-none/release/pool.wasm`, relative to the repository root.
To build that artifact without submitting a hosted job, run from the repository
root under the shared resource guard:

```sh
python3 scripts/verify_local.py --seconds 900 -- env CARGO_BUILD_JOBS=1 \
  just --justfile pool/justfile build
```

Do not substitute `make build` for the instrumented build: the Make recipes do
not enable `certora`. Both use the checked `release` profile, not the separately
defined `release-soroban` profile.

## Optional hosted execution

**Requires explicit authorization for the selected rules and hosted cost.**
Sunbeam execution requires a Certora account, a billable hosted service and
`CERTORAKEY`; it is not part of `make verify-local` or the repository CI gate.
Building locally or editing documentation does not authorize a submission.

In an isolated Python environment, install the CLI version used for the archived
configuration interface (`certora-cli==8.19.2`). Supply `CERTORAKEY` through the
environment without recording it in commands, source files or artifacts.
Run an explicitly approved configuration from `pool/`:

```sh
# Only after hosted execution is authorized and CERTORAKEY is set:
cd pool
certoraSorobanProver confs/oracle.conf
```

Run from `pool/` because `"build_script": "./certora_build.py"` resolves against
the working directory. Other configurations are `user_rules.conf`,
`pool_status1.conf` through `pool_status4.conf`, `health.conf`,
`health_unresolved.conf`, `utilization.conf`, `supply_cap.conf`,
`supply_cap_collateral.conf` and `supply_credit.conf`. A configuration's existence
does not imply a completed or successful proof; consult
[VERIFICATION.md](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/VERIFICATION.md) before selecting it.

## Reading verdicts and preserving evidence

- Read **each rule's** verdict in `output/<userId>/<jobId>/output.json`.
  Job-level `SUCCEEDED` means execution completed, not that every rule passed.
  `UNKNOWN`, `TIMEOUT`, cancellation, halted execution and missing output are not
  successful proofs; an undecided result is not itself a counterexample.
- Inspect required reachability checks and `rule_sanity` results separately.
  The latter may appear only in the hosted report's tree view, not in archived
  per-rule JSON. A SUCCESS verdict alone does not establish non-vacuity.
- Inspect selected rules, `optimistic_loop`, `loop_iter`, feature gates and
  summary dependencies alongside verdicts, using the central report's scopes.
- Preserve the original `output.json` bytes in `verification/certora/`; do not
  rewrite an old result to represent a later source tree. Keep source commit,
  lockfile, configuration, CLI/prover identity and artifact hashes with any new
  evidence so its inputs and dependencies remain identifiable.
- The [archive index](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/verification/certora/README.md) locates historical runs;
  its historical narrative is not the current status authority. Read archived
  JSON together with [VERIFICATION.md](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/VERIFICATION.md), not as an independent
  certification of the current tree.
- Authorized monitoring may need the `anonymousKey` retained under
  `pool/.certora_internal/`. Keep credentials and internal submission data out of
  committed artifacts.
