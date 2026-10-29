# Deployment registry

One JSON-lines file per network — `testnet.jsonl`, `mainnet.jsonl` — listing
every contract Templar deploys from the Blend fork stack and every WASM hash it
uploaded. The frontend reads these files straight from `main`:

    https://raw.githubusercontent.com/Templar-Protocol/precept/main/registry/testnet.jsonl
    https://raw.githubusercontent.com/Templar-Protocol/precept/main/registry/mainnet.jsonl

The deployer in `tools/abtest` (`precept-abtest market deploy`) writes the
files through `registry/registry.py` when it finishes a deployment, and the
`pool-deploy` workflow opens that record as a PR; for
anything else, curators add deployments by PR: append your record with
`registry/registry.py add` (it also normalizes the file for you), or edit the
`.jsonl` directly and CI's `check` will tell you what is wrong. CI runs the
unit tests and `check` on every PR, and verifies the files against live chain
state whenever `registry/**` changes.

Reusing an uploaded code hash (the point of the `wasm` lines): find the
`wasm` line for the artifact/version you need and deploy from it — no upload:

    stellar contract deploy --wasm-hash <hash> --network testnet

## Format

One canonical JSON object per line: keys sorted, compact separators (`:` `,`),
UTF-8, file ends with a newline. Lines are sorted with all `contract` lines
first by `(deployment, role, id)`, then all `wasm` lines by `(artifact, hash)`.
No duplicate unique keys: `(network, id)` for contracts, `(network, hash)` for
wasm. `add` rewrites the whole file canonically, so hand-editing order or
spacing is never required, just not wrong-casing hex or breaking sort.

### `kind: "contract"`

    {"deployment": "...", "id": "C...", "kind": "contract", "network": "testnet",
     "refs": {...}, "role": "pool", "status": "active", "wasm_hash": "<64 hex>",
     "deployed_at": "YYYY-MM-DD"}

- `id` — contract strkey; `role` — what the contract is: `pool`, `backstop`,
  `pool_factory`, `pool_oracle_adapter`, `proxy_runtime`, `proxy_governance`,
  `lazer_source`, `price_source`, `oracle`, `token`, `comet`, `emitter`,
  `faucet`, `admin_smart_account`, `admin_policy`.
- `deployment` — the stable name of the run that created the contract (its
  evidence run name). Contracts a later run reused keep the creating run's
  name; when the run that *created* them is retired but they still serve an
  active stack (shared comet/emitter/tokens), they stay `active`.
- `status` — `active` or `retired`. An active contract may not `refs` a
  retired one; `retire --deployment D` enforces that when it flips a group.
- `wasm_hash` — the on-chain executable hash (sha256 of the WASM bytes);
  `null` only for contracts we did not upload (e.g. Stellar asset SACs).
  Every non-null `wasm_hash` must have a matching `wasm` line in the file.
- `refs` — role → contract id for the relationships (a pool refs its
  `pool_oracle_adapter`/`backstop`/`pool_factory`; an adapter refs its
  `proxy_runtime`; a runtime refs its `proxy_governance` and vice versa;
  sources ref their verifier/runtime). Targets must be registered, except
  under external keys `verifier` and `token` (e.g. the Pyth Lazer verifier,
  third-party tokens), which may name contracts we did not deploy.
- optional: `label` (free text, e.g. which token a token contract is),
  `admin` (G… account or C… contract strkey, e.g. a multisig smart account),
  `assets` (pool reserve token contract ids).

### `kind: "wasm"`

    {"artifact": "pool", "hash": "<64 hex>", "kind": "wasm", "network": "testnet",
     "source": {...}, "version": "v0.1.1"}

- `hash` — the on-chain WASM hash; deploy instances with
  `stellar contract deploy --wasm-hash <hash>`.
- `artifact` — what the code is (`pool`, `backstop`, `pool_factory`,
  `pool_oracle_adapter`, `soroban-proxy-oracle`, `soroban-proxy-governance`,
  `soroban-lazer-source`, `soroban-proxy-batcher`, `token`, `comet`,
  `emitter`, …). The same artifact may have several hashes/versions.
- `source` — how to get the bytes: `{"repo", "tag", "asset"}` for a GitHub
  release asset, `{"repo", "commit"}` for a reproducible build, or a `note`
  saying why the source is not pinned (e.g. a crate's prebuilt testutils
  wasm, a recovered qualified-bytes build). `version` is the release tag, or
  `unreleased` for local builds, or the crate's version for crate-bundled
  prebuilts. Optional: `length` (byte size).

## CLI

Run from the repository root (or pass the absolute path to `registry.py`
from anywhere — it locates the registry files next to itself, not by cwd):

    python3 registry/registry.py add    --network testnet records.jsonl  # or - for stdin
    python3 registry/registry.py add    --network testnet records.jsonl --update
    python3 registry/registry.py retire --network testnet --deployment my-run-20261027
    python3 registry/registry.py check
    python3 registry/registry.py verify --network testnet|mainnet [--rpc URL]
    python3 registry/registry.py check-config configs/*.json --network testnet|mainnet [--rpc URL]

`add` validates each record, merges by unique key (identical record = no-op;
a conflicting one is an error unless `--update`) and rewrites the file in
canonical form. `retire` sets `status: retired` on a deployment's contracts.
`check` validates every file — schema, strkey checksums, hex, canonical form
and order, uniqueness, refs and wasm-hash resolution, statuses — and exits
non-zero with one precise message per problem. `verify` is read-only: it
queries `getLedgerEntries` on the network's Soroban JSON-RPC (default: the
public testnet RPC, or the keyless public `mainnet.sorobanrpc.com`; `--rpc`
overrides either) and confirms
each active contract's executable hash and that every `wasm` hash is live
code on that network. It is deliberately not a CLI wrapper: same ledger
entries as `stellar contract info`, no dependency to install. `check-config`
also reads the chain, for nothing but the admins; see Configs.

The deployer (`tools/abtest`) emits, per deployed contract, the `contract`
record above (with `refs` filled from the deploy arguments) and, per uploaded
code, the `wasm` record with `source` pointing at the release or commit it
built from; it then calls `add` (with `--update` only when it intentionally
repins a record) and `retire` when it replaces a deployment.

## Configs

**These pools are not live. Do not deposit into, borrow from or loop against any
pool described here until its contracts appear in `mainnet.jsonl` and the launch
is announced.**

`configs/<name>.json` holds a pool's launch parameters before it is deployed:
the `PoolFactory.deploy` arguments under `pool`, each reserve's
`queue_set_reserve` `ReserveConfig` under `reserves` (in index order), and the
`set_status` value to activate with, plus free-text `notes` and `oracle`: per
reserve, the price sources, quorum (`min_sources`) and freshness (`max_age_secs`). Field names and units are the
contract's own; `i128` values are decimal strings, as the `stellar` CLI takes
them. A `null` is a value not yet decided. Configs are inputs, not records: once the
pool is live its contracts go in `<network>.jsonl` as usual. `check` does not
read this directory; `check-config` does:

    python3 registry/registry.py check-config registry/configs/*.json --network mainnet

It is the gate a pool-config PR has to pass before the deployer is run. One
precise line per problem to stderr, exit 1; a clean file prints one summary
line saying the network and whether the pool **reuses a runtime**
(`oracle.runtime` set) or **deploys its own** — in the latter case the run
pauses at the refresh phase until the new Lazer sources are pushed, so say so
in the PR. It checks the JSON parses, `network` equals `--network`, no value
is still `null` (an undecided value is not deployable; `pool.oracle` is exempt
because the deployer fills it), and every bound below. Factors and rates are
the contract's 7-decimal fixed point, so `10000000` is 1.0; the bounds live
in `POLICY` at the top of `registry.py`. They are deliberately conservative —
outside them usually means a decimal or units mistake, not a market decision,
and every shipped config sits well inside them:

| Bound | Why |
| --- | --- |
| `c_factor <= 0.95` | a 5% haircut at minimum; a liquidation still leaves the pool solvent |
| `l_factor <= 1.0` | a liability can't be counted as more collateral than it is |
| `0 < util < max_util <= 0.99` (borrowable reserves, i.e. `l_factor > 0`) | a target below the borrow ceiling, and borrows always stop short of 100% utilization; collateral-only reserves are exempt |
| `supply_cap`, `min_position` decimal i128 strings in `1..=i128::MAX` | the CLI takes i128 as decimal strings; zero means dust or a forgotten value; `i128::MAX` is the accepted "no cap" |
| per oracle asset `1 <= min_sources <= len(sources)` | a quorum nobody can satisfy, or a single price trusted as a quorum |
| per oracle asset `60 <= max_age_secs <= 86400` | under a minute thrashes the keeper, over a day serves stale prices as fresh |
| `backstop_take_rate <= 1.0` | a take rate above 100% is a units mistake |
| `1 <= max_positions <= 30` | positions cost ledger bytes; 30 is far above any real loop |
| reserve `index` values are `0..n-1` in list order | `queue_set_reserve` takes the index from the config |
| every reserve `label` has an `oracle.assets` entry | a reserve without a price cannot be counted as collateral |

`pool.admin` (and `oracle.admin`, if present) must be a checksum-valid `G…`
account or `C…` contract strkey **that exists on that network** — checked
with `getLedgerEntries` against the network's public RPC (`--rpc` overrides),
an account entry for `G…`, a contract-instance entry for `C…`. A curator
that does not exist yet cannot accept the handoff, and discovering that after
the deploy costs a day.

### Deploying a config

A curator opens a PR that adds or changes `configs/<name>.json`, with
`pool.admin` (and, if different, `oracle.admin`) set to their own account or
smart account. CI does the rest:

1. **Plan (`pool-plan.yml`, every PR push).** The deployer
   (`tools/abtest`, `market deploy --robot`) reads the chain, freezes the
   deployment plan and comments it on the PR with its `plan_sha256`,
   together with the `check-config` verdict. Nothing is sent and no secret is
   used, so this also runs for PRs from forks. The workflow runs the base
   branch's code; the PR's config is read as data only.
2. **Review.** Templar reviews the config and the plan; the merge is the
   approval of exactly that digest.
3. **Deploy (`pool-deploy.yml`, on merge).** In the protected `testnet` or
   `mainnet` environment, the same command runs with `--approve-plan
   <digest>`. The deployer re-checks the digest before every send and refuses
   a plan that changed (for example if the network moved under it); then
   open a new PR touching the config to get a fresh plan (a merged PR cannot
   be re-opened, and a plan binds its PR head). A refresh step stops the run until the
   pool's price feeds are live; the comment says how to resume
   (`workflow_dispatch`, `run=<id>`); settled steps are never sent again.
4. **Handoff.** The same run proposes the pool admin to `pool.admin`,
   transfers ownership of each Lazer source it deployed to `oracle.admin`,
   grants the governance `Admin` role of a runtime it deployed to
   `oracle.admin` and revokes its own, and sets the Comet controller to
   `pool.admin` last. A registry PR records the contracts.
5. **Accept.** The curator signs `accept_admin()` on the pool (within 10 days)
   and `accept_ownership()` on each Lazer source from their own wallet; the
   deploy comment lists each call. Activation (`set_status`) and every later
   parameter change are the curator's own admin actions: CI never activates.

## Tests

    python3 -m unittest discover -s registry
