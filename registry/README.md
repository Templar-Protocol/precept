# Deployment registry

One JSON-lines file per network — `testnet.jsonl`, `mainnet.jsonl` — listing
every contract Templar deploys from the Blend fork stack and every WASM hash it
uploaded. The frontend reads these files straight from `main`:

    https://raw.githubusercontent.com/Templar-Protocol/precept/main/registry/testnet.jsonl
    https://raw.githubusercontent.com/Templar-Protocol/precept/main/registry/mainnet.jsonl

Our deploy tooling (private `precept-internal` repository) writes the files
through `registry/registry.py` whenever a precept checkout is available; for
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
  `faucet`.
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
  `admin` (G… strkey), `assets` (pool reserve token contract ids).

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
    python3 registry/registry.py verify --network testnet [--rpc URL]

`add` validates each record, merges by unique key (identical record = no-op;
a conflicting one is an error unless `--update`) and rewrites the file in
canonical form. `retire` sets `status: retired` on a deployment's contracts.
`check` validates every file — schema, strkey checksums, hex, canonical form
and order, uniqueness, refs and wasm-hash resolution, statuses — and exits
non-zero with one precise message per problem. `verify` is read-only: it
queries `getLedgerEntries` on the network's Soroban JSON-RPC (default: the
public testnet RPC; mainnet has no public RPC, pass `--rpc`) and confirms
each active contract's executable hash and that every `wasm` hash is live
code on that network. It is deliberately not a CLI wrapper: same ledger
entries as `stellar contract info`, no dependency to install.

The precept-internal tooling emits, per deployed contract, the `contract`
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
`set_status` value to activate with, plus free-text `notes` and, once known,
`oracle_sources` (per-reserve price feeds). Field names and units are the
contract's own; `i128` values are decimal strings, as the `stellar` CLI takes
them. A `null` is a value not yet decided. Configs are inputs, not records: once the
pool is live its contracts go in `<network>.jsonl` as usual. `check` does not
read this directory.

## Tests

    python3 -m unittest discover -s registry
