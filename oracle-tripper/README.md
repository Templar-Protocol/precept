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
set_caller(env, caller: Address)      // only `owner`; rotates the caller key
rearm(env, asset: Address)            // only `owner`; re-arms the asset's one-shot trip budget
is_armed(env, asset: Address) -> bool // true while `trip` can still fire; true for a never-tripped asset
get_owner / transfer_ownership / accept_ownership / renounce_ownership   // OpenZeppelin Ownable, two-step
extend_ttl(env)                       // permissionless; renews instance and code
```

Errors raised by this contract: contract error `1001`
(`TripperError::Spent`) when the asset's one-shot budget is already spent.
1001 is deliberately outside the governance `GovernanceError` range (1-11)
and the runtime's `ContractError`, whose codes propagate unchanged (see
Failure modes).

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
`SetRole(<tripper>, ManualTripper, true)` proposal.

## Verify before relying on it

- `governance()`, `caller()` and `get_owner()` return the intended addresses.
- `is_armed(<asset>)` is true for every asset that was never tripped.
- Governance `has_role(<tripper>, ManualTripper) == true`.
- Governance `get_operation_ttl(SetManualTrip) == 0`.
- A simulate-only `trip` (`--send no`) from the caller key succeeds. This
  is mandatory before the role grant is relied on.
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
  owner re-arms it; recovery is untripping each asset. The spent marks stay
  until the owner `rearm`s them.
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
`rearm(<asset>)` — one signing round, like `set_caller`. `is_armed(<asset>)`
reads the budget back; it is true for a never-tripped asset.

This is the **tripper's** `rearm`, distinct from governance's circuit-breaker
`Rearm` action; runbooks say "tripper re-arm" for this one. A governance
call that fails inside `trip` reverts the spent mark with the rest of the
invocation, so a refused trip does not consume the budget.

## Rotation

The owner calls `set_caller(<new key>)`. The old key loses the power in the
same call; the governance role stays on the tripper, so no redeploy and no
`SetRole`. On mainnet the owner is the Admin smart account, so this is one
2-of-3 signing round. `transfer_ownership` / `accept_ownership` move the owner
in two steps; `renounce_ownership` freezes the caller for good.

A full replacement (new tripper) still means revoking the old one first
(`SetRole(<old tripper>, ManualTripper, false)`), then deploying and granting.

## Keep alive

`extend_ttl()` renews instance and code. Add the tripper to the keeper's
`extend_ttl_contracts` batch.
