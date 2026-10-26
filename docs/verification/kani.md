# Kani operational guide

Use [`VERIFICATION.md`](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/VERIFICATION.md) for authoritative results, proof
assumptions, scopes and limitations. This guide explains how to run and maintain
Kani proofs; a historical receipt does not establish a result for a new checkout.
Use the [shared local gate](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/VERIFICATION.md#reproducing-this) to run the
combined Kani/Verus acceptance set.

## Toolchain

Use **Kani 0.68.0** (bundled rustc 1.100.0-nightly).
Install Kani outside the workspace so its dependency resolution is independent:

```sh
(cd /tmp && cargo install --locked kani-verifier --version 0.68.0 --force)
test -d "$HOME/.kani/kani-0.68.0" || cargo-kani setup
cargo kani --version
```

## Preparation and execution

Run from the repository root after the [shared Wasm preparation](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/VERIFICATION.md#reproducing-this).
`contractimport!` consumes built Wasm at compile time. Clear `RUSTC_WRAPPER` for
Kani: the `sccache` compiler probe is incompatible with `kani-compiler`.

```sh
make verify-local-codegen
python3 scripts/verify_local.py --seconds 900 -- env RUSTC_WRAPPER= \
  cargo kani -p pool --lib list --format json
python3 scripts/verify_local.py --seconds 900 -- env RUSTC_WRAPPER= \
  cargo kani -p pool --lib --exact \
  --harness pool::reserve::verification::prove_b_token_forward_floor \
  --solver kissat -Z unstable-options --harness-timeout 600s
```

Code generation checks compilation, not assertions. The listing command writes
`kani-list.json` (gitignored); use it to recover fully qualified selectors.
Always use `--exact`: without it, `--harness` is a substring filter and can
select generated cases as well as the named harness. Reserve frame selectors
include `pool::reserve::verification::frames::`, not just `verification::`.
Prefer Kani's harness timeout to an external kill, which may leave no verdict.

The required PR jobs in
[`pull_request.yml`](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/.github/workflows/pull_request.yml) compile the pool proof
surface and verify [`kani-ci-harnesses.txt`](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/verification/kani-ci-harnesses.txt)
with Kissat and a 600-second per-harness budget. Each harness runs separately;
the checker consumes its actual exit status and complete log:

```sh
python3 scripts/check_kani_ci.py \
  --selection verification/kani-ci-harnesses.txt \
  --harness pool::reserve::verification::prove_b_token_forward_floor \
  --log /path/to/that-harness.log --exit-code 0
```

Pass the real process status, not a constant zero, when automating execution.

The weekly/manual
[`verification-full.yml`](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/.github/workflows/verification-full.yml) selects
[`kani-full-harnesses.txt`](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/verification/kani-full-harnesses.txt), one exact
harness per matrix job, with a default 10800-second budget. Despite the filename,
this is the expensive selected subset, not the complete released inventory.
The [cover inventory](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/verification/kani-expected-covers.json) must match the
union of both selections. CI requires exact cover counts, including zero, and
rejects missing, extra, malformed or unsatisfied summaries.

## Reading verdicts

Retain the complete log, exact selector, source/configuration identity, tool
versions, solver and budget. Expect one `VERIFICATION:-` line per selected
harness; a missing or extra verdict is not success. Inspect more than exit status:

- `VERIFICATION:- SUCCESSFUL` establishes the assertions under that harness's
  assumptions. Check every required `cover!` separately: unreachable covers do
  not make Kani fail, and may expose a vacuous assertion path.
- `VERIFICATION:- FAILED` with a counterexample trace (`Trace for` or
  `Counterexample`) requires investigation as a possible refutation.
- `CBMC timed out` records budget exhaustion, not a counterexample.
- `invariant violation` or `failed with status` can indicate a prover crash.
  A killed run or an unexplained failure is inconclusive, not a pass.

Do not weaken assertions, domains, expected covers or production behavior to
obtain success. A changed source digest is a new proof candidate, not an extension
of an old receipt.

## Maintaining proof connections

Proof bodies are under `pool/src/proofs` and `backstop/src/proofs`, attached to
production modules behind `cfg(kani)` gates. Preserve selectors when reorganizing
them. Reuse the production arithmetic seams; audit any alternate proof arithmetic
against the runtime implementation before transferring a result.

When changing supply accounting, review the post-mint guards in
[`actions.rs`](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs), the mint/burn callers and paired
position-transfer decreases in [`user.rs`](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/user.rs), and the
writer-enumeration tests. Accrual can raise accounted supply above the configured
cap without minting shares: maintain the action-admission boundary, not a global
cap invariant. The supplier-side delta harnesses in
[`reserve.rs`](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/proofs/reserve.rs) drive mutators, not token settlement;
use `VERIFICATION.md` for their domains and the remaining induction obligations.
Writer enumeration is syntactic: alias writes and struct-literal initialization
need separate review. Preserve frame assertions and nontrivial covers when adding
reserve fields or mutation paths.

## Optional release inventory and publication

The historical proof package is available from
[`kani-run-753b1ae-2026-09-15`](https://github.com/Templar-Protocol/precept/releases/tag/kani-run-753b1ae-2026-09-15).
Download its `.tar.zst` and `.sha256` assets into a fresh directory, run
`sha256sum --check kani-run-753b1ae-2026-09-15.tar.zst.sha256`, extract into a fresh
readback directory, then run `sha256sum --check SHA256SUMS` inside the extracted
package. Check the source binding in `VERIFICATION.md` before using its results.

For an authorized future publication, supply a proof package containing `kani.py`,
`inventory.json`, `original-obligations.json` and
`integrated-execution-receipt.json`, plus the run directory and referenced logs:

```sh
make kani-release KANI_RELEASE_DRY_RUN=1 \
  KANI_RELEASE_COMMIT="$(git rev-parse HEAD)" \
  KANI_RELEASE_PACKAGE_DIR=/path/to/proof-packaging \
  KANI_RELEASE_RUN_DIR=/path/to/integrated-run
```

The target runs publisher tests, reconciles selectors/verdicts/covers/source maps,
rejects unsafe input ownership, permissions and file types, and verifies a
local archive readback. Only remove `KANI_RELEASE_DRY_RUN` when publication is
authorized: that creates a tag and GitHub release and verifies downloaded assets.
Use exclusive authority or protected `kani-run-*` tags. Archive integrity and
custody checks are not cryptographic execution attestation against a malicious
operator; use a separately signed attestation if that threat is in scope.
