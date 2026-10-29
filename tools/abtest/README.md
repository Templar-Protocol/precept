# precept-abtest

Deploys two Blend pools on Stellar testnet and drives identical step sequences against
both, in lockstep, through the `stellar` CLI. Every divergence is classified as Expected
(per pair profile) or Violation; evidence is retained per step; `report.md` gives a verdict.

Pairs: pinned stock Blend v2 vs the ADR-0008 security fork (`pairs/stock-vs-fork.toml`),
or two fork pools with different reserve configs (`pairs/fork-vs-fork.toml`).

## Building

Always inside the repo's devenv shell, which pins Rust 1.98.0 to match the contracts:

```
devenv shell -- bash -c 'cd tools/abtest && cargo build --release'
```

`Cargo.lock` is committed and resolved against that pin — `cargo build --locked` and
`cargo test` are the gate. The graph still carries pins chosen for older MSRVs
(`clap` held at `=4.5.20`; `instability`, `darling`, `unicode-segmentation` fixed):
harmless on 1.98, load-bearing for reproducibility. Regenerate the lockfile only
deliberately, never as a side effect of a toolchain bump.

## Requirements

- Pair drives: a `stellar` CLI **>= 23** on `PATH` or via `--stellar-bin` / `STELLAR_BIN`.
  Market explorer commands need **>= 26.1.0**. The `stellar` on `PATH` from the devenv
  shell (28.1.0) is the same binary the shell exports as `PRECEPT_STELLAR_BIN`; set
  `STELLAR_BIN="$PRECEPT_STELLAR_BIN"` for raw cargo invocations.
  The pool-oracle-adapter has its own frozen artifact-producer CLI pin; see
  `pool-oracle-adapter/`.
- Optimized artifacts from `precept` (the Blend v2 fork): fork-side from `make build`
  under `target/wasm32v1-none/optimized` (Rust 1.98, soroban-sdk 25.3.0); the stock
  baseline separately under `target/adr8-base/target/wasm32-unknown-unknown/optimized`.
- Mock oracle/token/emitter Wasm from the cargo registry (paths in `pairs/*.toml`).

## Use

Every drive selects exactly one isolated scenario family, on a fresh deployment made for
that family. There is deliberately no `all`: a deployment shared between families cannot
attribute its state to either of them.

```
export STELLAR_BIN="$PRECEPT_STELLAR_BIN"   # devenv exports its pinned CLI
cargo run -- --out runs/$(date +%s) deploy --pair pairs/stock-vs-fork.toml
cargo run -- --out runs/<ts> run --scenario lending,interest,statuses,backstop,stress,freeze
cargo run -- --out runs/<ts> run --scenario strict_oracle
cargo run -- --out runs/<ts> run --scenario liquidation,stale_auction,direct_default
cargo run -- --out runs/<ts> report
```

Families — one `deploy` and one `--out` each:

- `lending,interest,statuses,backstop,stress,freeze`
- `strict_oracle`
- `liquidation,stale_auction,direct_default`: `stale_auction` needs `liquidation` and
  `direct_default` needs both, driven in that order.

`--scenario` must be the canonical comma-separated spelling of one family: no duplicates,
no reordering, no cross-family mixing. A run dir that already drove one family refuses a
second before it saves any scope. `freeze` is last: status 4 is absorbing. Nine trap
probes run after each scenario. Stock outcomes are recorded rather than required to
match the fork's declared rejection; failed calls must still be definite, unchanged,
and event-free. Stock trap mutations (including reward-zone changes) remain findings,
not waived differences. A completed execution is not a passing report.

The oracle family repays per-side debt before resetting a future-dated price, so later
steps do not inherit the intentional borrow spread. Replay credits mint supply only
from valid successful SEP-41 events emitted by the manifest's own token contract.

Negative control: copy only `manifest.toml`, `state.json`, and `evidence/` to an offline
scratch directory, change the copied profile to `fork-vs-fork`, and report that copy.
Trap and bad-debt differences must fail; never edit the original campaign or copy its
identity directories.

`--out` holds identities (`.stellar/`, funded by friendbot on first use and reused),
`manifest.toml`, `state.json`, `pending/` and
`evidence/<scenario>/<NNN>-<label>/{a,b,diff}.json`. Evidence is immutable: a completed
step's directory is never overwritten, and `deploy` refuses a run dir that already holds a
deployment.

`run` resumes by skipping a completed label only while its canonical evidence is still
readable and still judges clean. `--from-step <label>` changes only where the scan starts;
it cannot re-drive a completed label. A `pending/` journal whose chain outcome was never
settled makes the whole run dir refuse to start: the tool never auto-recovers or replays a
mutation whose outcome is unknown, so that deployment is spent and you deploy fresh.

The step barrier is recomputed from retained evidence on every resume, never from a stored
verdict. A step whose recorded invocation contradicts its expectation — or whose
invocation record was deleted — blocks every later step in its scenario instead of being
trusted as done. Nothing is waived by an invariant message: profile-specific accounting
assumptions come only from the typed `invariant_policy`, and there is no allow-list of
historical failures.

## Finding and inspecting runs

```
precept-abtest list                                    # runs, how far each got, who owns one
precept-abtest tui --watch                             # newest run, read-only
precept-abtest --out runs/x tui --scenario strict_oracle   # drive: needs a family
```

`--out` is optional everywhere and defaults to the most recent run under `./runs`.
A run is only driveable by one process at a time (`run.lock`); a second one opens
read-only, which is what `--watch` forces.

## Operator terminal

```
precept-abtest --out runs/<ts> tui --scenario strict_oracle   # drive: owns the run lock
precept-abtest --out runs/<ts> tui --watch                    # read-only follow/browse
```

Driving requires the same explicit family selection as `run`; `--watch` may omit it.

Watch checks the authoritative `a.json` and `b.json` file metadata, not the derived
`diff.json`, and reloads changed records. Missing or unreadable records are removed
from the current view. Read failures stay visible beside navigation/search and prevent
a PASS verdict; readable neighbors remain browsable. The next poll retries only the
file read, never a chain call. Do not replay steps or edit evidence to clear a warning.

Four panels over the selected step: `[1]` the live ADR invariant ledger (the same
rows as `report.md`, updated as steps land), `[2]` the A/B diff tree coloured by
Match / Expected / Violation, `[3]` a JSON drill-down of either side's snapshot,
`[4]` the stream of `stellar` CLI calls and contract events.

Keys: `space` pause, `s` single step, `n` skip, `R` re-run the selected step as a
disclosed operator action, `g` jump, `:` command palette, `p` force a snapshot, `a`/`b`
side, `d` diff-only, `/` search, `?` help, `q` quit.

There is no retry key. A step that produced evidence is finished history, and a step whose
submission outcome is unknown must not be re-sent: neither can be replayed from the TUI.

Transient RPC failures (502, timeouts, `ResourceLimitExceeded`) retry with backoff
inside the CLI wrapper and show a counter in the header. That is transport-level only: a
mutating call is never replayed on an error that could have happened after submission, so
a transaction cannot double-apply. Semantic failures, a wrong outcome or a broken
invariant, halt the run; you may then resume, skip forward, or stop, but not re-run what
was already applied.

The palette runs ad-hoc operations through the same path the scenarios use, so an
operator action is recorded in `evidence/operator/` and checked like any other step.
Those steps are excluded from the pass/fail verdict and disclosed separately in
`report.md`: a deliberate one-sided action desyncs the pair by design.

Only one process may own a run directory (`run.lock`, pid-based); a second invocation
opens read-only. Exit codes: 0 pass, 2 fail verdict, 1 tool failure.

For automation, `run --json` emits one JSON object per event instead of the TUI. With no
operator present a halt is final: the run stops there and a tool failure exits 1.

`cargo test` runs offline against a scripted fake executor and ratatui's `TestBackend`;
it needs no terminal and no network. The one registry test that compares the
emitter against a real deployment is `#[ignore]`d, so a machine without the
evidence can never report it green; where those trees exist, set
`PRECEPT_EVIDENCE_DIR` and `PRECEPT_CHECKOUT` and add `-- --ignored` to the
`cargo test` command.

## Disposable protected market

`market` is a separate run. It does not change the paired A/B commands or
their evidence. `--market <lab.toml>` runs the testnet lab graph; `--config
<pool.json>` with `--network` deploys a real market from a precept pool config
on the network named in that config, covered under "Real market from a precept
pool config". The minimum supported Stellar
CLI is **26.1.0**.
The devenv shell exports its pinned CLI as `PRECEPT_STELLAR_BIN`; pass
`--stellar-bin "$PRECEPT_STELLAR_BIN"` for direct invocations. CLI 26.0.0
rejects the explorer's `--source <G> --sign-with-key <alias>` signing form,
so it is refused before any network request or pending-send record.
A lab graph run selects a testnet RPC. The selected RPC must itself report the
exact Stellar testnet passphrase, and passing a testnet signing passphrase to a
mainnet RPC is refused.

If your shell exports RPC or network defaults, add
`--rpc-url https://soroban-testnet.stellar.org --passphrase 'Test SDF Network ; September 2015'`
before `market` on these commands. Do not depend on an inherited environment
from a different network.

`market deploy` and `market resume` in headless mode also require an
interactive terminal for each confirmation; input typed before a gate is
displayed is discarded. A pipe of preloaded `y` responses cannot authorize
deployment.

### Robot mode

A non-interactive driver — an agent, a CI job — runs the same deployment with
`--robot` on `market deploy`, `market resume`, or `market deploy --resume`. It
never reads stdin; the object it approves is the run's own frozen plan file,
not a line of typed input. Two phases:

```
# 1. freeze the plan and stop: one JSON line {"event":"plan",...} carries the
#    run directory, the sha256 of deployment-plan.toml, and every coordinate;
#    then the first pending confirmation's summary, and exit 3. Nothing is sent.
precept-abtest --out runs/<ts> market deploy --config <pool.json> \
  --network mainnet --admin-identity <alias> \
  --admin-config-dir ~/.config/stellar --robot

# 2. approve exactly that file and continue: every event is one JSON line
precept-abtest --out runs/<ts> market deploy --config <pool.json> \
  --network mainnet --resume --admin-identity <alias> \
  --admin-config-dir ~/.config/stellar \
  --robot --approve-plan <plan_sha256 from phase 1>
```

The lab graph recurses the same way: phase 1 is `market deploy --market
<lab.toml> --robot` and phase 2 is `market resume --market <lab.toml> --robot
--approve-plan <digest>`.

`--approve-plan` must be that 64-hex sha256. The plan file is frozen by the
deployer before the first confirmation, so it is always on disk when either
phase reads it; the digest is re-derived from `deployment-plan.toml` before
*every* send, so a plan edited or replaced after approval stops the run at the
next confirmation with `{"event":"approval_refused",...}` and exit 4, before
anything is sent. Events are `phase`, `confirm` (summary only), `custody`,
`done`, `failed`, plus `plan`, `approval_refused` and `stopped`; the
confirmation token is never printed, and a `failed` event carries the
`violations` the stopping step reported. Exit codes: `0` finished, `1` stopped
/ failed / custody unresolved, `3` plan printed and awaiting approval, `4`
approval refused. `--approve-plan` without `--robot` is a usage error.

The approval covers **the whole run, across resumes**. A digest names one
frozen plan file, not a point in progress, so the same digest releases every
confirmation of every resume of that plan — there is no separate approval per
resume. What changes between resumes is the state, not the plan: before each
robot resume, run the same command with `--robot` and **without**
`--approve-plan`. That prints the current `plan` line again — the same
`plan_sha256`, with each step's `settled` flag now true for the coordinates the
run has finished — and exits 3 without sending. Read it, then resume with
`--approve-plan <digest>`.

A step that runs to a definite refusal stops the run: it is never recorded as
done. The live case is a `refresh` whose asset has no Accepted price yet. That
coordinate's intent is archived under `market-unapplied/` (with the violations
that stopped it) and the run exits 1, so the next resume asks the same
coordinate again once the condition holds — a refresh is idempotent, and a
refusal is re-askable. An open refresh intent is reconciled on resume only when
the runtime actually caches an Accepted price for that asset; anything less
stays unresolved rather than settling.

In `--robot` mode **every line on stdout is one JSON object**. The human
account of the run (network, pool, reserves, planned coordinates, registry
notices) goes to stderr, so a consumer parses stdout without filtering.

`market deploy` refuses a lab market config unless every scheduled reserve
(`stable`, `xlm`, `weth`) sets all ten parameters explicitly: `c_factor`, `l_factor`,
`util`, `max_util`, `supply_cap` (base-unit string), `r_base`, `r_one`,
`r_two`, `r_three`, `reactivity`. The fixture defaults in `src/deploy.rs`
exist for paired runs only; a lending market may not inherit an unreviewed
cap or rate curve. `markets/protected-testnet.toml` spells those defaults out
and is a template, not a reviewed risk profile. After each `set_reserve`, and
again before the pool leaves setup status, the deployer reads `get_reserve`
back and refuses on any field that differs from the config. `status`,
`explore` and `call` do not apply this gate.

Each `[artifacts.<role>]` table of a lab market config is one of two pin forms.
The path form
(`{path, sha256}`) deploys an existing local file, unchanged. The release form
(`{release, version?}`) resolves the role from a published
Templar-Protocol/contracts release: `release` is the catalog artifact key, and
only `proxy-runtime` → `soroban-proxy-oracle` and `proxy-governance` →
`soroban-proxy-governance` have a release form. With no `version`, the newest
*catalogued* release is chosen — only versions with a CI-recorded catalog row
on contracts `dev` (`contract/artifacts/releases/<key>@<version>.tsv`) are
eligible; a tagged-but-uncatalogued release is never used, and an exact
version without a row is refused. Bytes are downloaded from that GitHub
Release and enforced against the row (exact byte length and SHA-256) before
any use; verified files are cached at
`$XDG_CACHE_HOME/precept-abtest/artifacts/<sha256>.wasm` and re-verified on
every use. Resolution happens once, at deploy start: the exact version,
digest, tag and catalog commit are frozen into the run's
`deployment-plan.toml`, and `market resume` uses those frozen values — it
never re-resolves `latest`. `--artifact-version <role>=<version|latest>`
(repeatable; release-form roles only) is accepted by `market deploy` and
`market resume`; for a resumed run only the frozen version, or `latest`
naming it, is accepted — any other exact version is refused. There is no
fallback to a local build or an unverified file on any failure.

### Recording deployments in the precept contract registry

`market deploy` and `market resume` record a completed market in the precept
deployment registry (`registry/` in this repository) once the
deployment has finished. The location resolves in order: `--registry <precept-checkout>`, else
the `PRECEPT_REGISTRY` environment, else the precept checkout the command runs
in; when none has a `registry/registry.py` the step skips
with one log line and the deployment is unaffected. `--no-registry` opts out;
the two flags cannot be combined.

Recording is additive and never rewrites a completed deployment. Two files
land in the run dir, each written atomically. `<run>/registry-records.jsonl`
is the complete record of what the run described, built from the run directory
alone and written before the registry is read; `<run>/registry-add.jsonl`
holds the rows to submit, and is the file
`registry/registry.py add --network testnet <file>` is given. Rows the
registry already describes stay in the first file only, as do the contracts a
replacement reuses: they belong to the deployment that created them, and that
one is retired with `retire --network testnet --deployment <replaced>` only
when the replacement references none of its contracts — `registry.py` retires a
whole deployment or nothing, so a base whose comet, emitter or tokens are still
in use is reported as staying active instead of being asked for a refusal.

Failures come in two kinds. Before that first write — pins that cannot be
read, a run dir that cannot be written — the step exits non-zero and names no
file, because none exists. After it — an unreadable table, a refused `add` —
the error names the complete record and the file to re-`add`, and the
completed deployment is left untouched either way, so the operator can fix the
cause and re-run `add` by hand.
Nothing is committed or PR'd to precept automatically; the tool prints the
exact `git -C <precept> …` and `gh pr create` steps. Plans, dry-runs and
differential `pair` runs never record, and `market` never fabricates a wasm
source: code with frozen release provenance records that release, path-pinned
code records nothing the registry could not describe better by hand.

```
precept-abtest --out <run> --stellar-bin <stellar-26> market contracts
precept-abtest --out <run> --stellar-bin <stellar-26> market inspect source.0 set_data
precept-abtest --out <run> --stellar-bin <stellar-26> market query adapter lastprice --arg 'asset={"Stellar":"<stable-contract-id>"}'
precept-abtest --out <run> --stellar-bin <stellar-26> market status
precept-abtest --out <run> --stellar-bin <stellar-26> market events pool --start-ledger <deployment-ledger> --count 20
precept-abtest --out <run> --stellar-bin <stellar-26> market receipt <transaction-hash>
precept-abtest --out <run> --stellar-bin <stellar-26> market tui --mode explore
```

`contracts` lists the immutable manifest roles. `inspect` reads the deployed
ABI, including UDT input shapes. `query` simulates a call without a send.
`status` reports individual reserves, actors, prices and custody, and exits
nonzero when the market is not ready. `null` means no current price, not zero.
`events` returns one bounded page with an opaque `next_cursor`; an empty page
is not proof of complete history. `receipt` never attributes an older,
identical transaction to a newer attempt.

Run `market guides` for optional source, governance, pool, token, backstop and
auction recipes. They resolve methods against the **live** ABI and use the
same guarded call as an unrestricted `market call <role> <method>`. Supply
native CLI argument values as repeated `--arg name=value`, not XDR:

```
precept-abtest --out <run> --stellar-bin <stellar-26> market guide oracle publish-price \
  --asset stable --source 0 --arg price=10000000 --arg timestamp=<ledger-seconds> \
  --signer <alias> --signer-config-dir <stellar-identity-dir>
```

That command only previews. Add `--execute` **in an interactive terminal**
to review the exact target, signer address, arguments, live ABI, simulation,
network and fee disclosure, then type `y`. The confirmation is single-use
and never printed. Signer secrets never belong in flags. CLI `--fee 10000`
is an inclusion bid in stroops, **not** a cap on the additional resource fee.
The read-only explorer works without signing flags. For actions, pass both
`--signer <alias>` and `--signer-config-dir <stellar-identity-dir>`. In the TUI,
`a` opens the curated Actions catalog; `/` filters it, arrows browse, and Enter
selects an action. The guided form derives its fields and types from that recipe's
live ABI. Manifest-bound values are read-only; asset and deployed source selectors
are visible. Oracle publication/refresh and manual breaker proposals use the live
proxy-runtime registered asset list, intersected with this manifest's token IDs;
they never default to or offer BLND unless the runtime actually registers it.
Token actions retain all manifest token choices, while pool gulp offers only
manifest reserves. Malformed/empty live registries and empty token/reserve selectors
fail closed and cannot produce a guided form or preparation capability. The inspector shows
every available manifest-bound choice. Address choices show a trusted run-local name
beside the exact public ID; defaults are visibly labeled `admin`. Enter opens the shared
autocomplete editor for ABI-typed address fields: type a name/prefix or G/C ID, Up/Down
inspects choices, Tab completes, Enter commits the concrete public ID, and Esc discards.
Opening shows the current ID and blank query; Enter on a blank query leaves it unchanged.
An exact name or unique prefix resolves directly; an ambiguous prefix stays open until
you visibly select a choice. Any checksum-valid public G/C literal is accepted.
`n` on a selected editable address saves its current valid value under a name in the
run-local `market-addresses/` store; duplicate names cannot overwrite. These aliases
store public addresses only and grant no signer identity or signing authority.
Editable `Request.request_type` leaves use the guided request-type autocomplete rather than
the generic integer editor. Enter opens a numbered list (`0 Supply` through
`9 Delete liquidation auction`); type a numeric opcode or label prefix, use Up/Down to
highlight a choice, Tab to complete it, and Enter to commit. For example, `Borrow` and `4`
both commit integer opcode `4`. Ambiguous prefixes such as `Supply` stay open until you
select a choice; invalid input such as `10` stays open with a diagnostic. Opening with a
blank query and pressing Enter, or pressing Esc, leaves the prior value unchanged. The
accepted domain remains `0..=9`, values stay JSON integers, and recipe-locked auction
request types remain immutable.

Open faucet: the `token faucet-mint` recipe mints any amount of a manifest
token to any address on testnet once a deployed open faucet is the token's
admin — no admin signature, the faucet itself calls `mint` and Soroban
authorizes the contract. Attach it once with
`market attach faucet <CONTRACT_ID>`, then select `token / faucet-mint`: the
token is bound from the manifest and only `to` and `amount` are yours; the
guide refuses while no `faucet` role is attached. The direct `token mint`
recipe fails once the token's admin is the faucet. The faucet's owner can hand
admin back with `set_token_admin`.

The original shared market's five tokens use faucet
`CBYD3LKQIOVCZJPOKSWH7QRAQXTPCFMZBITL6CEKJW3QLASWELASCJR2` (owner: the market
admin). In your own run directory for that market, attach it with
`market attach faucet CBYD3LKQIOVCZJPOKSWH7QRAQXTPCFMZBITL6CEKJW3QLASWELASCJR2`.

`Left`/`Right` can still cycle guided address choices. Enter on a vector header
(for example `pool/submit` → `requests`) creates its first row if empty and selects
the first editable child; Enter again edits that field. Re-entering a populated
vector or a structured record selects its existing fields without adding a row.
Select the vector header and use `+`/`-` to add/remove further rows.
Recipes with a selectable asset or oracle source show `asset` and `source` rows
above the typed fields; the form opens with the first typed field selected, so press
Up to reach them. Enter opens a filterable chooser of eligible assets (for example
`weth`) or deployed sources (`source.0`…): type to filter, Up/Down to highlight, Tab
to complete, Enter to commit, Esc to cancel. A changed choice reloads the live form
once; a blank query or the same choice leaves it unchanged. Empty ABI vectors are explicit `[]`; only auction request
recipes require at least one row. Empty or invalid required scalar/record fields block
simulation and preparation with a field diagnostic. A rejected typed edit stays open
with its draft intact: correct it and press Enter, or press Esc to discard the draft
and keep the last committed value. While editing, `r`/`p`/`y` are text, not actions:
editing and recovery hints replace the inactive navigation and action shortcuts.
Normal action hints return after a valid save or Esc; neither restores a prior
confirmation. Use Ctrl-C to quit while editing, including below the minimum
terminal size.
Rejection returns the inspector to the top, with the field diagnostic ahead of
other context so it remains visible while editing in compact terminals.
`r` simulates one complete call; `p`
prepares one guarded call; inspect and scroll through the complete per-call target,
arguments, simulation, signer, network, and fee disclosure before `y` confirms once.
From a guided result, `r` or `p` repeats that same recipe with the retained form;
`Esc` returns to the populated form for editing. The explorer has no bulk-send or
auto-confirm control.
Menu/form changes and navigation invalidate any prepared authority. `y` without a
fresh preview cannot send.
Use Tab to return to ABI browsing, `s` for the read-only status dashboard, `t` to
fetch a receipt, and `q` to quit. The first status visit reads the market; later
visits reopen the last snapshot without fetching. Press `s` **inside the status
page** to refresh explicitly. There is no background polling or automatic refresh
after a transaction: refresh before recording current readiness or post-state
evidence. Status arrows browse sections/fields/values, Tab switches panes, Enter
inspects, PgUp/PgDn page, Home/End jump, Left/Right scroll long values, and Esc
returns. The pinned readiness/reason summary stays visible while browsing; cached
status is display-only and never enables sends.

Fresh status collection validates the testnet identity first, then fetches live
ABIs and independent read-only observations in batches of at most eight concurrent
CLI jobs. Transient failures, including HTTP 429 rate limits, retry only the
affected contract read: at most five attempts, with 1, 2, 4, then 8 seconds of
backoff. Successful sibling reads are not replayed; exhausted or permanent
failures remain visible as unavailable observations, not fabricated readiness.
The identity probe remains single-attempt and fail-closed. Transaction preparation
and submission remain serialized; status collection never sends transactions.

The Governance section lists oracle-governance `next_proposal_id`, the pending
`active_ids`, and each pending proposal's operation, creator, creation time, and
TTL, plus the manifest token it targets (for example `weth`) when the operation
names one. Executed or cancelled proposals leave the pending list. At most the 16
newest pending proposals are shown (`proposals_shown` appears when more exist).
An unreadable id list omits `proposals` instead of implying an empty queue.
Governance is display-only and never changes market readiness.

For `oracle/publish-price`, choose the asset and source, then enter `price` as a
positive integer in smallest units (the display price multiplied by `10^decimals`;
for 7 decimals, `10000000` means `1.0`), with no floating-point input. Review the visible
current host-seconds `timestamp` default before simulation; host time is not ledger
time. Source replay, proxy freshness, and clock-drift guards remain authoritative.
In the explorer, each oracle source and each call is confirmed separately; it
does not synthesize quotes or quorum.

For `pool/set-emissions-config`, the typed `res_emission_metadata` list starts
empty (`[]`), which is a valid ABI input. Leave it empty and press `r` to probe the
live contract guard. Enter on the empty header, or `+` on the header, adds a row
with `res_index` (u32), `res_type` (u32), and `share` (u64). The Precept fork rejects
this operation outright with `BadRequest (1200)`; an empty list does not clear
emissions. The recipe uses the normal live-ABI simulation/preparation path.

The TUI can only report failures preserved by the pinned CLI/contract error surface:
known Blend rejection codes (including 1000/1200) show their names and safe help.
Authentication, transport, malformed, and infrastructure failures are explicitly
not contract-rejection proof. Successful and ambiguous outcomes are not inferred;
unresolved custody blocks further sends and must never be replayed.

A known error from `r` is only the pinned CLI's no-send simulation result. It does
not prove that a signed transaction was submitted, that a disabled action is
impossible on-chain, or that post-state remained unchanged; this workflow records
no intent for a failed simulation and performs no chain write.

In the form, address fields are chosen from visible manifest actors or entered as
a public Stellar address; signer configuration is used only by the guarded sender,
never read to guess an actor. Pool requests expose contract kinds 0–9 as typed rows.
`4` (borrow), `2` (supply-collateral), and `5` (repay) are real pool calls, not
storage setters. Auction fill/delete recipe kinds are fixed by their recipe.
Backstop/auction/request amounts are exact base units (no floating-point
conversion). Flash-loan contract, asset and amount are separately inspectable.
The unrestricted generic ABI browser remains available for direct inspection;
guided recipes never accept operator-assembled JSON.

Status is one bounded snapshot: reports over 64 KiB or observations over 4 KiB
are rejected whole (never truncated); journal inspection is capped at 256
coordinates. The dashboard exposes only captured fields and labels these source
limits rather than implying omitted data was recovered.

Every confirmed `abtest` send reserves a durable intent **before** one CLI attempt.
The run lock excludes a simultaneous deployment, resume, play or other
prepared action; finish that driver before a manual call, including an
emergency governance action. A transport-ambiguous send stays unresolved
and blocks subsequent actions. Never replay it or use an older matching
receipt as settlement. Inspect `market status` and reconcile public chain
evidence first.

Keep the run directory writable only by its trusted operator. Journal digests
and no-follow readers detect malformed or redirected files; they do **not**
authenticate a result against a writer with the same filesystem access as the
operator. A matching `result.json` is not independent chain proof. For an
ambiguous send, preserve the open intent and reconcile the actual transaction
and post-state externally before any further send. Do not synthesize a result
file to make `journal-clear` appear.

When public chain data proves an attempted send was never applied, settle the
open coordinate instead of leaving it blocking forever. Take the signer
account's transactions that bracket the attempt (e.g. the account's Horizon
history), then run:

```sh
precept-abtest --out runs/<dir> market settle <PHASE>-<SLOT> \
  --consumed-by <hash1> --consumed-by <hash2> ...
```

The tool re-proves the exact testnet, fetches each hash's envelope and
metadata, and requires every one of them to be a v1 transaction signed by the
frozen intent signer whose operations never invoke the intent target, with
consecutive sequences and no gaps, the first closing at or before `T0-60 s`
and the last at or after `T0+3390 s` (`T0` = the intent file's mtime). The
bound is the confirmed send's start deadline (the two key lookups, each allowed
`RETRY_ATTEMPTS` attempts of the 300 s wall plus the `1+2+4+8 s` backoff), plus
the send's own 300 s wall, plus 60 s of clock slack. A confirmed send refuses to
start once that deadline has passed on the real-time clock, so a stalled
journal sync or a sleeping machine before the send cannot void the bound. That
proves the attempt's account sequence lies inside the verified range, so it can
never land. Only then is the journal settled as `not applied` through the
existing writer, with every consumed hash kept in the record. Any doubt — a gap,
a target-invoking transaction, a wrong source, an unknown envelope, or coverage
that does not bracket the attempt — refuses and writes nothing.

The executor rechecks that deadline after its file setup, immediately before
launching the send child. What the bound cannot cover: the child itself
starting late or the machine sleeping inside it before it reads its account
sequence. If that may have happened, do not settle. Closing it fully needs the
transaction's sequence recorded before sending.

`market replay <PHASE>-<SLOT> --consumed-by <hash> --consumed-by <hash> ...`
is the same proof for a coordinate of a **real** run that is still partial (a
frozen plan, no manifest). It runs on the network the frozen plan names, and the
coordinate is archived under `market-unapplied/` rather than settled, so
`market deploy --resume` runs it again. The window rules differ because a real
coordinate is not always an invocation. Every window transaction must still be a
v1 transaction signed by the intent signer with consecutive sequences and the
same time bracket, but whether a transaction *could be the attempt* depends on
what the coordinate submitted:

- **Deploy.** The attempt is one contract creation installing the coordinate's
  declared Wasm. A window transaction is refused if it creates a contract from
  that Wasm (`create_contract` or `create_contract_v2`, under any id preimage or
  salt) or from an executable the reader cannot classify. Classic operations
  (`create_account`, `change_trust`, `payment`, `set_options`, ...), uploads,
  invocations of other contracts, creations from other Wasm and Stellar Asset
  Contract creations cannot be it, so a run's own phase-2 transactions do not
  block the proof. A deploy that declares no digest (a classic asset's contract)
  refuses only on an unclassifiable creation. Whatever the window shows, the
  predicted contract's instance is then read from the ledger
  (`ledger entry fetch contract-data --contract <predicted> --instance`): only
  the CLI's own answer for a missing contract counts as absent, and it spells
  that answer two ways — exit 1 with `contract not found: <that exact id>` on
  stderr (seen on testnet) or exit 0 with an empty entry list (seen on mainnet)
  — while an existing contract, any other failed read (network error, rate
  limit, "not found" naming another id) or a malformed answer refuses. A
  deployment that landed is never archived.
- **Config / invoke.** A window transaction is refused if it invokes the
  coordinate's target. Classic operations and creations are unrelated to an
  invocation.
- **Upload, funded key and anything else** are settled by their own laws and are
  refused here.

An operation of a shape the reader does not know, a v1 envelope it cannot parse,
or a fee-bump envelope refuses the read itself: unknown shapes fail closed.

Each encoded journal record is capped at 65,536 bytes. A guarded call whose
intent or conservative settlement envelope exceeds that limit refuses before
reservation or send. If a future outcome unexpectedly exceeds the cap after a
submission, its result is not truncated or written: the retained intent stays
unresolved. Preserve it and reconcile public receipt, events and state before
any new action; never replay the submission or fabricate a settlement.

The publication coordinate is the one exception to that external-reconciliation
instruction, because it submits nothing: publishing `market-manifest.toml` is a
local write, so an open intent there is an interrupted write rather than an
ambiguous send. Resume completes it from the file itself, and only when the
published bytes reproduce the run's own settled facts, including its pool. It
never replays a mutation and never rewrites the frozen intent.

`market status` inspects at most 256 pending custody coordinates in one pass.
A run holding exactly 256 settled coordinates reports `journal-full`; a run
with more than 256 reports `journal-truncated`. Both states refuse every
guarded send **before** a new intent is written, because one more journal
entry would fall outside the bounded scan and could never be verified again.
These are capacity limits, not corruption: the stored records are intact and
nothing is deleted.

If `market status` reports `journal-full` or `journal-truncated`:

1. Stop sending from that run directory immediately.
2. Preserve the old run's evidence and verify it. Do not prune
   `market-pending/`, `evidence/`, or any other journal or evidence file:
   deleting them destroys the only settlement proof. Copy the whole run to
   durable storage if you need to archive it.
3. Funded holders must reconcile their positions with public chain evidence
   before abandoning the old run. Never replay an ambiguous submit; confirm
   what actually landed with `market receipt` and `market events`.
4. Redeploy a **new** disposable run directory with its own market config and
   new manifest (`market deploy --market <market-config.toml>` into a fresh
   `--out`). Never relink, rename, or copy the old run's journal into the new
   directory; leave the old directory in place byte-for-byte. A fresh run
   under the limit reports `journal-clear` while the old evidence stays
   untouched.

The synthetic publisher is under `contracts/price-source/`. Deploying a market
or opening the explorer does not install a continuous quote publisher. Quotes
expire. Publish each source/asset deliberately, refresh through the proxy only
when its guards allow it, and confirm distinct funded actor identities and
positions before claiming a market is ready. The dedicated keeper below applies
only to the original spoon; other disposable deployments may still have stale
quotes and collapsed actor identities.
Moving its source into this repository did not replace the already deployed
Wasm; the rebuilt artifact has a matching interface but different bytes.

## Real market from a precept pool config

`market deploy` deploys a real market from a precept pool config, the
`precept/registry/configs/*.json` shape with an `oracle` block. The flag decides
which path runs: `--market <lab.toml>` runs the testnet lab graph described
above, unchanged, and `--config <pool.json>` together with `--network` deploys a
real market. The two flags are exclusive. Both paths go through the same
journaled, resumable, gated executor and the same progress output, and both
write to the run directory named by the tool's existing `--out`.

```
precept-abtest --out <run> market deploy --config <pool.json> \
  --network <testnet|mainnet> --admin-identity <alias> --admin-config-dir <dir> \
  [--activate] [--wasm-dir <dir>]
precept-abtest --out <run> market deploy --market <lab.toml> \
  --admin-identity <alias> --admin-config-dir <dir>
```

`--network` belongs to `--config` only and is refused unless the pool config
names the same network. `--activate` and `--wasm-dir` apply to a real market.

`pool.oracle` is an optional existing-adapter override. A pool config without
it gets one oracle proxy configuration per asset from the `oracle` block of the
config.

A real run records the following in its manifest: the network label, every
contract id, the per-asset oracle-proxy configuration, the synthetic-leg
issuers and their classic asset codes, and the launch artifact pins used with
where each came from. A pin came from either an operator-declared `live` pin or
a local build whose bytes hashed to that digest.

A pool launch is per-pool isolated. Each pool gets its own backing pool, its
own price adapter, its own pool contract, and its own synthetic-leg issuers and
assets. Runtime and governance are the only shared contracts. A config naming
them causes reuse, never a redeploy; a first launch on a network that lacks
them uploads and deploys them.

`mainnet` is a network a config may name and a value the operator passes on the
command line. No mainnet send has happened. The testnet rehearsal is
unfinished.

### Handing the market over

A pool config may name an owner that is not the key running the deployment.
`pool.admin` is the address the pool belongs to when the run is over, and
`oracle.admin` the address the oracle contracts belong to; each is a strkey, an
account (`G…`) or a contract (`C…`). The deploying key still signs every send —
the configuration says who the market ends up belonging to, not who pays for
the transaction. A target that is not on the network at all, an account with no
ledger entry or a contract that has never been deployed, is refused at preflight
before anything is sent: the run would only meet it at the end.

Nothing is handed over when both fields name the deploying key, and a config
written before these fields existed names nothing, so those runs get the graph
they always got, coordinate for coordinate. When something is owed, one more
phase runs after activation, in this order: the pool's `propose_admin` to
`pool.admin`, a two-step `transfer_ownership` of each Lazer source this run
deployed to `oracle.admin`, then — when this run deployed the runtime and its
governance — a governance proposal granting `Admin` to `oracle.admin` and a
second removing it from the deploying key, and last the backing pool's
`set_controller` to `pool.admin`.

Each coordinate first asks whether its target already holds what would be
handed to it, and settles without sending when it does; an ownership proposal
that is already open is this run's own work, not a second proposal to make. The
two governance halves are skipped as a pair. Nothing here is complete on the
chain: `propose_admin` and `transfer_ownership` ask the new owner to accept, and
the acceptance is what a curator signs. A finished run therefore ends with the
list of calls still owed — the contract, the method, and the key that must sign
it — printed for a human and carried as `handoff.accept` in the `done` event for
a robot, so a driver can tell "deployed" from "deployed and waiting for the
curator". An empty list means the market is fully its own.

## Shared `just` recipes

`justfile` in this directory wraps the common market commands. Run `just` here,
or `just -f tools/abtest/justfile <recipe>` from the repo root; recipes always run
in this directory and never load a `.env`.

```sh
just build                          # release binary; inside the devenv shell
just check                          # offline tests, Clippy -D warnings (the CI gate)
just market-status                  # exit 1 means NOT READY
just market-contracts
just market-assets
just market-query pool get_config
just market-query token.usdc balance --arg 'id=G...'
just market-explore                 # unsigned
just market-explore-signed my-alias # sends still need p then y
```

Set these first; market recipes refuse with a message when one is missing:

| Variable | Needed for | Value |
|---|---|---|
| `PRECEPT_MARKET_RUN` | every `market-*` recipe | your own run directory (below) |
| `PRECEPT_STELLAR_BIN` | every `market-*` recipe | a Stellar CLI **>= 26.1.0** by exact path (devenv exports it) |
| `PRECEPT_SIGNER_CONFIG_DIR` | `market-explore-signed` | the Stellar config directory holding your key |
| `PRECEPT_RPC_URL`, `PRECEPT_PASSPHRASE` | optional | default to the public testnet |

Each person uses their own run directory and their own keys:

- To use an existing market, ask its operator for its `market-manifest.toml`. It
  holds public contract addresses only. Put it in a new directory with an empty
  `market-pending/` next to it. Do not copy another operator's `market-pending/`:
  that journal belongs to their sends.
- Fund and sign as your own account. Only the market's operator signs as the
  market admin. Two senders on one account race for its sequence numbers. A
  raced send can be left unresolved, which blocks every later send from that
  run directory.
