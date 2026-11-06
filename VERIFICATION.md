# Formal verification status

All seven of the safety invariants this protocol commits to are backed by
machine-checked proof, and across the whole campaign — three independent provers
and a fuzzer — **no counterexample has ever been produced and no rule has ever
been violated.** Where a prover did not finish it either exhausted its budget
or returned undecided, and those are different things; in neither case did it
find a way to break a property.

What each proof covers differs, and this document is explicit about it rather
than leaving it to the footnotes. Every invariant is proven over a stated scope
and under stated assumptions. Two are proven against a faithful model rather
than the live component. Solvency is the partial one: it covers three of the
seven ways the reserve book can change, and it bounds how those three move the
pool's position rather than establishing the `assets >= liabilities` inequality
outright. Two vacuous results are documented below: the Certora user-health
execution rule remains vacuous; the formerly vacuous Kani zero-debt accrual
harness now has a verified, reachable zero-growth proof.

This is the authoritative report for verification status, claim scopes and
limitations. Operational instructions are separate:
[`kani.md`](docs/verification/kani.md),
[`certora.md`](docs/verification/certora.md) and
[`verus.md`](docs/verification/verus.md). Archived Certora verdicts remain in
[`verification/certora/`](verification/certora/).

## The seven invariants

| Invariant | In plain terms | Prover | Result | Assumptions |
| --- | --- | --- | --- | --- |
| **Solvency** | The pool never owes depositors more than the assets backing them. | Kani | proven for 3 of 7 ways the book can change | **D**, **S** |
| **No free money** | You cannot extract value by converting between assets and pool tokens, in either direction. Rounding always favours the pool. | Kani | proven | **D**, **S** |
| **Interest accrual safety** | Interest only ever moves forward. Accrual cannot reduce a debt, and cannot pay out more interest than it charged. | Kani | proven | **D** |
| **Health factor monotonicity** | Adding collateral can only improve a position; borrowing more can only worsen it. No action has a perverse effect. | Kani | proven | **D**, **S** |
| **Borrow cap** | Deposits and utilization cannot exceed the limits the pool is configured with. | Kani | proven | **D**, **T** |
| **Liquidation correctness** | Liquidation auctions price, discount and time correctly, and bad debt is handled without creating value. | Kani + Certora | proven | **S** |
| **Oracle circuit breaker** | The pool refuses a price that is missing, zero, negative, future-dated or more than 24 hours stale. | Certora | proven, 13/13 rules | **T** |

The protocol has no borrow cap as such; it has a supply cap and a maximum
utilization, which is what the audit property list calls "borrow caps".

## What the assumption classes mean

A proof is only as strong as what it assumes. Every row above is graded against
the same three gaps, and each row names the ones that apply.

- **D — domain narrowed.** The property is proven exhaustively, but over a
  restricted range of inputs rather than every value the type allows. Typically
  balances from 0 to 255 and interest rates in hundredths up to 2.55×. The
  arithmetic is checked for *every* combination in that range, with no sampling.
  New Verus theorems establish supply/withdrawal bounds over unbounded mathematical
  integers. Applying those wider theorems to production Rust requires the
  checked-i128/I256/host refinement; the existing production proof domains remain
  as stated.
- **T — target substituted.** The property is proven about a faithful model of a
  component rather than the component itself. The oracle rules, for instance, are
  proven against a mock price feed that can return any value, which is the right
  way to test the pool's *reaction* to a bad price, but is not the live feed.
- **S — scope narrowed.** A component of the named property is proven rather than
  the whole property end to end. Liquidation correctness proves the auction
  arithmetic and decisions, not a full liquidation executed against a live pool.

No row is free of all three, so none is claimed as unconditional. That is the
honest shape of formal verification on a system this size, and naming the gap is
what makes each claim checkable.

## Where each row's gap lies

- **Solvency** is the partial row and the most important, so it is stated
  precisely. The reserve book changes in seven ways; three are proven — deposit,
  withdrawal (on its uncapped path) and interest accrual. Borrowing, repayment,
  a default absorbed into the exchange rate, and the backstop-credit payout are
  not covered. The pool's token balance is not a modelled quantity and no
  initially-solvent starting state is established, so what these proofs give is
  that the three covered transitions cannot degrade the pool's position — each
  bounds the change in what the pool owes by the amount of value moved. They do
  not establish the absolute `assets >= liabilities` inequality, which would
  need both the balance modelled and a base case.
  The supply/uncapped-withdrawal bounds use supplies and amounts 0–255 and
  positive supplier rates in hundredths up to 2.55×; withdrawal assumes enough
  shares exist to burn. Positive-debt conservation is checked at debt supplies
  1, 2 and 255, with supplier supply 1–255, prior credit 0–255, supplier rates
  0–2.55×, debt/growth rates 1–2.55× in hundredths, and take rates 0–99 whole
  percent. These are reserve-mutator results, not token-settlement or user/global
  share-total invariants. The [proof bodies](pool/src/proofs/reserve.rs) define
  the exact domains.
- **No free money** and **health factor monotonicity** are proven about the
  conversion and ratio arithmetic that every action routes through, not about the
  handlers that call it.
- **Borrow cap** is proven at the supply-action admission boundary over the
  restricted domain, not as a permanent `total_supply <= cap` invariant:
  accrual can legitimately increase valued supply above the cap. Paired
  decreases account for share transfers that do not mint net supply. The
  production-width supply-handler obligations remain undecided. Certora's
  utilization results cover four guard implications and five witnesses over
  supplies 0–10^18, rates 10^12–10^15 and `max_util < 10^7`; they exclude
  post-default supplier rates below unity and the actual submit utilization
  loop. `utilization_in_range` remains budget-limited. See
  [cap rules](pool/src/spec/cap_rules.rs) and [Kani proofs](pool/src/proofs/actions.rs).
- **Liquidation correctness** covers auction arithmetic and decisions, not full
  liquidation settlement.
- **Oracle circuit breaker** establishes six first-read guard properties and
  seven reachability checks. The production guard is exercised with empty
  caches and a globally fixed arbitrary modeled feed response; feed accuracy,
  per-call response changes and arbitrary cache histories are outside that
  result. The [rules](pool/src/spec/oracle_rules.rs) independently encode the
  86,400-second boundary, which must stay aligned with production.

## Evidence

| | |
| --- | --- |
| Kani (bounded model checking) | Historical full inventory: 110 proof harnesses plus 1,678 generated fixed-parameter instances; [campaign `753b1ae`](https://github.com/Templar-Protocol/blend-contracts-v2/releases/tag/kani-run-753b1ae-2026-09-15) passed 3,197/3,197 obligations at that source commit, not the latest checkout. The Kani 0.68.0 property-mapped rerun at `886e26f` recorded 35 SUCCESSFUL, 2 budget-limited and 0 counterexamples; the separate supply/withdrawal solvency pair also passed. |
| Certora Sunbeam | Archived auditor-rule-set reproduction: 24 SUCCESS, 2 UNKNOWN, **0 VIOLATED**. Fork-specific additions: oracle circuit breaker 13/13, utilization guards/witnesses 9/10. Raw verdicts and source provenance are indexed in [`verification/certora/README.md`](verification/certora/README.md). |
| Fuzzing | Mutation fuzzing and deterministic committed-seed replay share the full pool-fixture execution path. When totals fit i128, floor-rounded price valuations of `(supplier claims + credit)` must strictly exceed ceil-rounded price valuations of debt. Exact arithmetic preflight identifies numeric range exclusions; unexpected valuation failures are not skipped. This is not a cash-aware solvency proof. |
| Local strengthening | The local gate (`just verify-local`, run at the time as the identical `make verify-local` recipe) passed five Kani harnesses with all 21 required covers, plus nine Verus proof functions: three unbounded-integer supply/withdrawal theorems, four helpers and two witnesses. The local set includes the repaired zero-debt proof, checked contract consumption, and the production first-read price guard and decimals validation/cache/reset under modeled host interfaces. The ceil-mint negative control detected both intended failures. |
| Production arithmetic extraction | Charon/Aeneas extracted and translated the actual reserve conversions, dispatch and checked-i128/I256 branches: 23 transparent functions and 22 opaque functions. SDK/arithmetic refinement and Lean theorem checking remain open; translation is not a theorem verdict. |
| Third-party audit | See [`audits/`](audits/). Formal verification complements an audit; it does not replace one. |

The local acceptance set strengthens and rechecks selected existing results; it is
not a disjoint count to add to the published inventory. Its additional scopes are:

- The [Verus theorems](verification/verus/supply_delta.rs) quantify over unbounded
  mathematical integers: nonnegative supplies/amounts and positive rates;
  uncapped withdrawal requires sufficient shares, and capped withdrawal burns
  at most the existing supply. They are not restricted to the Kani numeric grid.
  Their production checked-i128/I256 refinement remains a separate obligation.
- The [checked supply contract and caller](pool/src/proofs/actions.rs) share the
  0–255/hundredths domain. The contract is consumed by the cap-decision proof,
  but the arithmetic sequence is a transcription, not the live admission,
  utilization and persistence path.
- The [local oracle harness](verification/local-host/lib.rs) compiles unchanged
  production `Pool` code with modeled maps, invocation-constant time, arbitrary
  feed responses and aborts. The accepted first-read and decimals/cache/reset
  rules cover the exercised two-key footprint, not arbitrary collection
  histories or the expanded price-cache/reset obligation.

Kani's proof arithmetic uses checked-i128 `FixedPoint`; the runtime `Env`
implementation also has an I256 fallback. Certora uses instrumented pool builds
and summaries; the health-execution dependency gaps are detailed below.
Its configurations use optimistic loop bounds (one by default, two for health
and supply-cap configurations). Host/runtime correctness, authorization
enforcement, ledger atomicity and deployment behavior are not established by
these proofs. Intentional failures of the mutated arithmetic model are negative
controls, not protocol counterexamples.

### What a passing verdict does not always mean

The Certora builder/flash-loan health obligations remain undecided; the archive
contains UNKNOWN and TIMEOUT verdicts for them. Kani's
`astra_grow_debt_exact_delta` and `prove_grow_debt_nonnegative_interest` exhaust
their budget. Neither outcome is a violation. The related component proofs,
including composed and fixed-supply checked-arithmetic accrual proofs, remain
valid within their own domains; instrumented arithmetic results are not
interchangeable with production checked-arithmetic results.

More importantly, a SUCCESS verdict can be *vacuous* — satisfied without ever
exercising the property it names. Two such results were identified; the zero-debt
case has now been repaired. Both original outcomes remain visible below:

- **`user_health_execute_submit`** (Certora) passes because its post-state always
  equals its pre-state, so a trivial disjunct satisfies the assertion. It is in
  the 24 SUCCESS column, but that rule's user-health obligation is unexercised.
  This is distinct from the Kani health-monotonicity proofs in the table above.
  The vacuous rule is inherited verbatim from the auditor's harness and is also
  a limitation of the June 2025 report's Verified status.
  The builder summary's soundness remains UNKNOWN, and the passing transfer
  summary checks concern position entries, not token conservation. Existing
  builder sanity rules call the summarized wrapper, so they do not repair this
  execution rule's reachability.
- **`prove_accrual_conserves_value_debt_000`** (Kani) historically satisfied 0
  of its 2 positive-growth reachability checks. The replacement zero-debt proof
  establishes zero accrued value and unchanged supplier claims/credit, with
  **4/4 covers satisfied** under Kani 0.68.0. This is an exhaustive checked-i128
  kernel result on the grid defined in the
  [harness](pool/src/proofs/reserve.rs): zero debt, supplier supply/credit 0–255,
  supplier rates 0–2.55× and debt/growth rates 1–2.55× in hundredths. Production
  I256 refinement remains separate.

Proof verdicts and reachability outcomes are recorded together so meaningful
passes remain distinguishable from vacuous ones. The local acceptance set enforces
its required covers and mathematical witnesses; a bare SUCCESS verdict alone is
insufficient.

## Continuous enforcement

The [PR workflow](.github/workflows/pull_request.yml) compiles the pool proof
surface and re-verifies the [fast selection](verification/kani-ci-harnesses.txt).
Its [reusable local gate](.github/workflows/verification-local.yml) additionally
runs the complete existing [local proof inventory](verification/local-proofs.json),
Verus proofs/negative control, deterministic fuzz-seed replay, fuzz regressions,
strict fuzz-package lint and mutation-target build. The aggregate gate rejects
failed or skipped jobs.

Both Kani selections require the exact per-harness cover counts in
[`kani-expected-covers.json`](verification/kani-expected-covers.json), including
intentional zero-cover harnesses. Missing, unexpected or unsatisfied covers fail.
The [weekly/manual workflow](.github/workflows/verification-full.yml) runs the
[expensive selection](verification/kani-full-harnesses.txt) and a bounded mutation
campaign; successful corpus growth is cached and failures retain reproducers.
It is not the entire historical inventory. Manual `scope=local` runs the same
proof/replay gate as PR CI without the expensive weekly campaign.

## Reproducing this

Use the repository [Rust/Wasm toolchain](rust-toolchain.toml) and Stellar CLI
28.1.0, as pinned in the PR workflow. For local acceptance, also provision the
pinned [Kani](docs/verification/kani.md#toolchain) and [Verus](docs/verification/verus.md#toolchain) tools.
Linux, Python 3.11+, a user systemd manager and working cgroup-v2 limits are required.
From the repository root, prepare Wasm imports once or after their inputs change:

```sh
python3 scripts/verify_local.py --seconds 900 -- env CARGO_BUILD_JOBS=1 just build
just verify-local-codegen
just verify-local
```

The gate runs regression tests, the exact local Kani inventory and the Verus
proofs/negative control. It requires complete verdicts, covers and witnesses,
not just prover exit zero. Proofs run serially under a 4 GiB process-tree memory
limit, zero swap, 256 tasks, CPU quota at most 200%, bounded runtimes and
whole-unit cleanup. Missing containment or failed cleanup is an error. Guarded
Cargo commands disable compiler wrappers so an external cache daemon cannot
compile outside the unit. Logs are in `target/verification-local/`.
On a cold cache, `verify-local-codegen` compiles each configuration under a
separate bounded deadline; it does not verify assertions or relax proof deadlines.

Fuzz regressions use the pinned [fuzz toolchain](test-suites/fuzz/rust-toolchain.toml)
(lint uses the root toolchain) and the same Wasm imports:

```sh
python3 scripts/verify_local.py --seconds 1800 -- just fuzz-lint
python3 scripts/verify_local.py --seconds 1800 -- just fuzz-test
python3 scripts/verify_local.py --seconds 1800 -- just fuzz-replay
python3 scripts/verify_local.py --seconds 1800 -- just fuzz-build
```

Replay reads [reviewed JSON seeds](test-suites/fuzz/regression-seeds), checks
acceptance, persisted positions and selected wallet deltas, and rejects
unevaluable checks. Direct `just fuzz-replay` emits JSON Lines on stdout;
the containment wrapper captures stdout/stderr in its reported log.

The tool guides describe other Kani selections and authorized Certora operation.
Certora requires hosted infrastructure and explicit submission authorization;
it is not part of the local gate. Optional Charon/Aeneas extraction uses the
[pinned driver commands](verification/extraction/evaluation.json), separately
from theorem acceptance.
