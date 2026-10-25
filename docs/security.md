# Security and audits

Precept is a fork of [Blend v2](https://github.com/blend-capital/blend-contracts-v2), so its security position has two
parts: what was audited upstream, and what was reviewed after the fork.

## The Guardian review of this fork

Guardian reviewed the Precept fork and its report is in this repository:
[Guardian — Templar — Blend Contracts V2 Security Review, October 2026](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/audits/Guardian%20-%20Templar%20-%20Blend%20Contracts%20V2%20Security%20Review%20-%20October%202026.pdf).
It covers the fork code — an initial review at commit `7a09b0e3d228` and a remediation review at `b385dfb5cf18`,
which together form the code released as v0.1.0 — and records 5 Low and 10 Informational findings. v0.1.1 carries
post-review changes on top, including the event-topic changes described on
[Changes from Blend v2](changes-from-blend-v2.md#events-with-their-current-topics).

Every finding except one was resolved. The single open item is **L-05**, on the price adapter's timestamp handling,
which Guardian marked **acknowledged**. The reported defect was in the adapter: it rounded the parent's timestamp down
to the start of the resolution bucket, which hid a future-dated parent price from the pool's own check. The adapter in
this repository now refuses a parent price dated ahead of ledger time
([pool-oracle-adapter/src/lib.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool-oracle-adapter/src/lib.rs#L193-L197)).
The recommendation itself is aimed at the parent proxy oracle, a separate contract outside this repository: let an
observation inside the freshness window replace a history head that is ahead of ledger time, so the head cannot
outlast the clock. Guardian notes that passing the parent's timestamp through unfloored would also restore the pool's
check, but on its own it blocks pricing for that asset until ledger time catches up. Read L-05 together with the price
rules on [Risks](risks.md#the-oracle-is-the-single-point-of-failure): the pool independently rejects future-dated,
stale, non-positive and wrongly-scaled prices.

## Upstream Blend v2 audits

The stock contracts this fork derives from were audited before it existed. Those engagements reviewed the upstream
codebase, not Precept's changes, and are reproduced here for completeness:

* [Code4rena × Blend V2 audit report](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/audits/Code4rena%20x%20Blend%20V2%20audit%20report.pdf)
* [Script3 × Certora — Blend v2, Formal Verification Draft v2 Report, June 2025](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/audits/Script3%20-%20Certora%20-%20Blend%20v2%20-%20Formal%20Verification%20Draft%20v2%20Report%20-%20June%202025.pdf)
* [Script3 × Certora — Blend v2, Security Assessment Draft v3 Report, April 2025](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/audits/Script3%20-%20Certora%20-%20Blend%20v2%20-%20Security%20Assessment%20Draft%20v3%20Report%20-%20April%202025.pdf)

The fork's own behavioural delta against stock is listed on
[Changes from Blend v2](changes-from-blend-v2.md), which is the right place to read what the upstream reports do not
cover.

## Reproducing a release build

Releases are published on the [releases page](https://github.com/Templar-Protocol/precept/releases) with the four WASM
artifacts (`pool.wasm`, `backstop.wasm`, `pool_factory.wasm`, `pool_oracle_adapter.wasm`) and a `SHA256SUMS` file.
Every published artifact can be rebuilt from the tagged source:

```bash
git clone https://github.com/Templar-Protocol/precept.git
cd precept
git checkout v0.1.1
make build
cd pool-oracle-adapter && just preflight && just optimize
```

`make build` uses the pinned toolchain in `rust-toolchain.toml`; the optimized pool, backstop and factory land in
`target/wasm32v1-none/optimized/`. `just preflight` is the adapter's own toolchain gate — it requires exactly Stellar
CLI 28.1.0 and Rust 1.98.0, and `just optimize` does not run it, so skipping it can produce adapter WASM from a
different toolchain whose checksum will not match. The adapter's optimized file is written to
`pool-oracle-adapter/target/wasm32v1-none/release-soroban-checked/pool_oracle_adapter.optimized.wasm`; rename it to
`pool_oracle_adapter.wasm` before hashing, as the release workflow does, and compare all four files against the
release's `SHA256SUMS`. The `just` recipes run through a `pool-adapter-env` wrapper that must be on `PATH`; in CI it is
a `exec "$@"` shim created by the release workflow
([.github/workflows/release.yml](https://github.com/Templar-Protocol/precept/blob/v0.1.1/.github/workflows/release.yml#L87-L92)).
That workflow rebuilds the optimized WASMs from the tagged source and fails if any rebuilt file's checksum differs from
the `SHA256SUMS` attached to the release.

To run the contract suite and the fork-vs-stock differential harness yourself, see
[Getting started](https://github.com/Templar-Protocol/precept/blob/v0.1.1/README.md#getting-started) in the repository
README.

## Formal verification

The pool carries machine-checked proof harnesses, and they run in CI:

* [Kani proofs](verification/kani.md) — bounded model checking over the pool's arithmetic and state transitions.
* [Certora runs](verification/certora.md) — rule- and summary-based checks against the contract model.
* [Verus theorems](verification/verus.md) — the pure kernel, proven in Rust.
* [VERIFICATION.md](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/VERIFICATION.md) — the
  proof surface, what each harness covers, and how to run them.

## Reporting a vulnerability

Report security issues to **security@templarprotocol.com**. Please give enough detail to reproduce the behaviour — the
contract address, the entrypoint, and the sequence of calls — and allow time for a fix before disclosing publicly.

Precept does not currently operate a bug bounty programme; a report is appreciated, not payable.
