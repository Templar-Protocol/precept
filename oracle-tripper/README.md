# oracle-tripper

One-call manual trip of a single asset's price feed on Templar's proxy-oracle
runtime. The trip normally needs two governance calls (`create_proposal` with
the exact next proposal id, then `execute_proposal`), which a fixed-input
submitter cannot do. `trip` does both in one transaction, so the keeper's
alert-sink route can pause an asset from one signed Hypernative alert with one
fixed call per asset. Each asset can be tripped once until the owner re-arms
it (see Re-arm).

Pinned to `rev eea0060d (governance v0.1.5, runtime v0.1.4)`. Only
`Asset::Stellar` feeds are reachable (all three mainnet feeds today).

## ABI

```text
__constructor(env, governance: Address, caller: Address, owner: Address)   // fails if `governance` has no next_proposal_id()
trip(env, asset: Address) -> u64      // trips Asset::Stellar(asset); only `caller`, once per asset until `rearm`; returns the proposal id
governance(env) -> Address
caller(env) -> Address
set_caller(env, caller: Address)      // only `owner`; rotates the caller key; emits a `caller` event [old, new]
rearm(env, asset: Address)            // only `owner`; re-arms the asset's one-shot trip budget
is_armed(env, asset: Address) -> bool // true while `trip` can still fire; true for a never-tripped asset
get_owner(env) -> Address | null
transfer_ownership(env, new_owner: Address, live_until_ledger: u32)
                                      // only `owner`; starts the two-step transfer;
                                      // the new owner must accept before `live_until_ledger`;
                                      // re-calling with 0 cancels the pending transfer
accept_ownership(env)                 // only the pending new owner; completes the transfer
renounce_ownership(env)               // only `owner`; permanent: `set_caller` AND `rearm`
                                      // are unreachable forever after (with no owner left,
                                      // every owner-gated call fails with 2100)
extend_ttl(env)                       // permissionless; renews instance and code
```

Errors that can surface from this contract (all as Soroban contract
errors). Read the diagnostic events, not the number alone — governance and
runtime codes share one numeric space (see Failure modes).

| Code | Source | Meaning / typical trigger |
| --- | --- | --- |
| 1001 | `TripperError::Spent` (this contract) | the asset's one-shot budget is already spent; the owner must `rearm` |
| 2100 | OZ `OwnableError::OwnerNotSet` | owner-gated call (`set_caller`, `rearm`, `transfer_ownership`, `renounce_ownership`) after `renounce_ownership`; permanent |
| 2101 | OZ `OwnableError::TransferInProgress` | `renounce_ownership` while a two-step transfer is pending |
| 2102 | OZ `OwnableError::OwnerAlreadySet` | `set_owner` (constructor path) with an owner already stored |
| 2200 | `RoleTransferError::NoPendingTransfer` | `accept_ownership` with no pending transfer, or cancelling (re-calling `transfer_ownership` with `live_until_ledger = 0`) when none exists |
| 2201 | `RoleTransferError::InvalidLiveUntilLedger` | `transfer_ownership` with a `live_until_ledger` in the past |
| 2202 | `RoleTransferError::InvalidPendingAccount` | a second `transfer_ownership` naming a different pending account |
| 2203 | `RoleTransferError::TransferExpired` | `accept_ownership` after `live_until_ledger` passed |

1001 is deliberately outside the governance `GovernanceError` range (1-11)
and the runtime's `ContractError`, whose codes propagate unchanged (see
Failure modes). The 21xx/22xx codes come from the OpenZeppelin `Ownable`
implementation (stellar-access `ownable`/`role_transfer`); `rearm` after a
`renounce_ownership` surfaces as `#2100`, not as an auth error.

## Effect of a trip

The runtime stops serving a price for that asset (cached reads and refreshes
are blocked). Every pool whose adapter prices that asset through this runtime
is affected: pools don't change status; their price-dependent operations fail
until the asset is untripped. That includes borrows, collateral withdrawals by
accounts with debt, and **liquidations**; suppliers without debt can still
withdraw. USDC is a reserve of both Templar mainnet pools, so tripping USDC
affects both. The mainnet assets are USDC, XLM and deJAAA.

## Deploy

```sh
stellar contract deploy \
  --wasm target/wasm32v1-none/optimized/oracle_tripper.wasm \
  --network <NETWORK> \
  -- --governance <GOVERNANCE_CONTRACT_ID> \
     --caller <TRIP_CALLER_KEY> \
     --owner <GOVERNANCE_ADMIN_SMART_ACCOUNT>
```

`<TRIP_CALLER_KEY>` is **our** key: it lives in our Secret Manager and the
keeper's alert-sink route signs the `trip` submissions with it. It is not a
Hypernative-held key.

## Grant the role

The governance Admin (today a 2-of-3 smart account) grants `ManualTripper` to
the **tripper contract address** (not the caller key) via a
`SetRole(<tripper>, ManualTripper, true)` proposal. Concretely, with the
multisig tool (`precept-internal` `tools/multisig/smart_account.py`;
`--arg` values follow its typed-JSON grammar):

```sh
# read the current id first:
stellar --network <NETWORK> contract invoke --contract-id <GOVERNANCE_CONTRACT_ID> \
  --source-account <ANY_PUBLIC_G> -- next_proposal_id
# then propose the create (2 signers of the Admin account):
python3 tools/multisig/smart_account.py propose --network <NETWORK> \
  --account <GOVERNANCE_ADMIN_SA> --contract <GOVERNANCE_CONTRACT_ID> \
  --fn create_proposal \
  --arg '{"address":"<GOVERNANCE_ADMIN_SA>"}' \
  --arg '{"u64":<NEXT_PROPOSAL_ID>}' \
  --arg '{"enum":["SetRole",{"address":"<TRIPPER_CONTRACT_ID>"},{"enum":["ManualTripper"]},true]}' \
  --arg '{"u64":0}' \
  --signer <G1> --signer <G2> --out grant-create.json
# sign + submit grant-create.json, then the same shape for
#   --fn execute_proposal --arg '{"address":"<GOVERNANCE_ADMIN_SA>"}' --arg '{"u64":<ID>}'
```

If another proposal lands between reading `next_proposal_id` and the create
being executed, the create fails (out-of-order); re-read and re-propose.

## Verify before relying on it

- `governance()`, `caller()` and `get_owner()` return the intended addresses.
- `is_armed(<asset>)` is true for every asset that was never tripped.
- Governance `has_role(<tripper>, ManualTripper) == true`.
- Governance `get_operation_ttl(SetManualTrip) == 0`.
- A simulate-only `trip` succeeds. This is mandatory before the role grant
  is relied on. Never put the caller's seed on a laptop: `--build-only`
  needs no signer at all — pass the caller's PUBLIC key (G…) as the source
  and pipe the built tx into `stellar tx simulate`:

  ```sh
  stellar --network <NETWORK> contract invoke \
    --contract-id <TRIPPER_CONTRACT_ID> \
    --source-account <CALLER_PUBLIC_G> --build-only \
    -- trip --asset <ASSET_CONTRACT_ID> \
  | stellar --network <NETWORK> tx simulate --source-account <CALLER_PUBLIC_G>
  ```

  Inspect the returned auth: it must record exactly one
  `SorobanAuthorizationEntry` for `<CALLER_PUBLIC_G>` whose root invocation
  is `trip(asset)` on the tripper. A successful simulation with no auth
  entry (or an entry for another address) means the call would not be
  authorized on chain.
- On mainnet, the governance and runtime code must be the registry's
  proxy-governance v0.1.5 and proxy-oracle runtime v0.1.4 rows
  (`registry/mainnet.jsonl`); this crate's ABI is pinned to rev eea0060d of
  those releases.
- Local gates: `cargo test -p oracle-tripper` and `cargo clippy
  -p oracle-tripper --all-targets -- -D warnings`. Clippy is a local gate
  only; CI runs none.

## Trip submission (keeper alert sink)

- Alerts arrive at the keeper's alert-sink route; the keeper verifies the
  alert signature against its config and submits `trip(asset)` signed by the
  `caller` key. The caller is **our** key held in our Secret Manager and read
  per action — not a Hypernative-held key — and this is not a Hypernative
  **Single Contract Call** action. The caller account must hold XLM for fees.
- One action per asset. Argument `asset` = the token contract address, and it
  must be one of the runtime's `registered_assets()` (as
  `{"Stellar": <address>}`); check it there rather than copying an example
  (mainnet USDC is `CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75`).

## Failure modes to monitor

- If another governance proposal lands between simulation and inclusion, the
  trip tx fails (footprint). The keeper must retry / re-simulate.
- A ProxyConfigurationManager holder can raise `SetManualTrip`'s TTL
  (governance-common `required_role`), which breaks the tripper (create +
  execute no longer fit in one tx).
- Governance caps pending proposals at 64.
- A compromised `caller` can trip each Stellar feed at most once until the
  owner re-arms it; recovery order is rotate (`set_caller`) → untrip →
  re-arm — see Rotation, never re-arm first. The spent marks stay until the
  owner `rearm`s them.
- A failed `trip` surfaces raw error codes from both governance
  (`GovernanceError`: 2 Unauthorized, 5 ProposalNotMature, 8
  ProposalOutOfOrder, 9 InvalidInput) and the runtime (`ContractError`, e.g.
  4 InvalidInput for an unregistered asset) in one numeric space; the
  tripper's own refusals are `TripperError::Spent` (contract error 1001).
  Read the diagnostic events, not the number alone.

## Untrip (human)

Governance Admin (today the 2-of-3 smart account), via the multisig signing
flow:

1. `create_proposal(<admin>, <next_proposal_id>, SetManualTrip(Stellar(<asset>), false, None), 0)`
2. `execute_proposal(<admin>, <id>)`

## Re-arm (owner)

`trip` spends a one-shot budget per asset: a second `trip(<asset>)` before a
re-arm fails with `TripperError::Spent` (1001) and creates no governance
proposal. The owner (the Admin 2-of-3 smart account) re-arms one asset with
one signing round — same tool and shape as `set_caller`:

```sh
python3 tools/multisig/smart_account.py propose --network <NETWORK> \
  --account <GOVERNANCE_ADMIN_SA> --contract <TRIPPER_CONTRACT_ID> \
  --fn rearm --arg '{"address":"<ASSET_CONTRACT_ID>"}' \
  --signer <G1> --signer <G2> --out rearm.json
```

`is_armed(<asset>)` is a pure read — no signing round:

```sh
stellar --network <NETWORK> contract invoke --contract-id <TRIPPER_CONTRACT_ID> \
  --source-account <ANY_PUBLIC_G> -- is_armed --asset <ASSET_CONTRACT_ID>
```

It is true for a never-tripped asset.

This is the **tripper's** `rearm`, distinct from governance's circuit-breaker
`Rearm` action; runbooks say "tripper re-arm" for this one. A governance
call that fails inside `trip` reverts the spent mark with the rest of the
invocation, so a refused trip does not consume the budget.

## Rotation

The owner calls `set_caller(<new key>)`; the old key loses the power in the
same call and a `caller` event `[old, new]` is emitted (monitor the event
stream for unexpected rotations). The governance role stays on the tripper,
so no redeploy and no `SetRole`. One signing round:

```sh
python3 tools/multisig/smart_account.py propose --network <NETWORK> \
  --account <GOVERNANCE_ADMIN_SA> --contract <TRIPPER_CONTRACT_ID> \
  --fn set_caller --arg '{"address":"<NEW_CALLER_G>"}' \
  --signer <G1> --signer <G2> --out rotate.json
```

`transfer_ownership(new_owner, live_until_ledger)` / `accept_ownership` move
the owner in two steps (re-call `transfer_ownership` with `live_until_ledger`
0 to cancel); `renounce_ownership` is permanent: `set_caller` AND `rearm`
become unreachable forever (every owner-gated call then fails with `#2100`).

**On a suspected caller compromise, rotate FIRST.** The order is:
`set_caller` (rotate) → governance Admin untrip (above) → owner `rearm`.
Never re-arm before rotating: a compromised caller keeps one trip per asset
until its mark is spent, and re-arming hands it another trip on every asset
you touch. Re-arming first lets the attacker trip the asset again before the
rotation lands.

A full replacement (new tripper) still means revoking the old one first
(`SetRole(<old tripper>, ManualTripper, false)`), then deploying and granting.

## Keep alive

`extend_ttl()` renews instance and code. Add the tripper to the keeper's
`extend_ttl_contracts` batch.
