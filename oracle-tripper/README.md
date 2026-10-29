# oracle-tripper

One-call manual trip of a single asset's price feed on Templar's proxy-oracle
runtime. It exists so a Hypernative **Single Contract Call** action (fixed
function, fixed arguments) can pause an asset: the trip normally needs two
governance calls (`create_proposal` with the exact next proposal id, then
`execute_proposal`), which fixed-input actions cannot do. `trip` does both in
one transaction.

Pinned to `rev eea0060d (governance v0.1.5, runtime v0.1.4)`. Only
`Asset::Stellar` feeds are reachable (all three mainnet feeds today).

## ABI

```text
__constructor(env, governance: Address, caller: Address)   // fails if `governance` has no next_proposal_id()
trip(env, asset: Address) -> u64      // trips Asset::Stellar(asset); only `caller`; returns the proposal id
governance(env) -> Address
caller(env) -> Address
extend_ttl(env)                       // permissionless; renews instance and code
```

## Effect of a trip

The runtime stops serving a price for that asset (cached reads and refreshes
are blocked). Every pool whose adapter prices that asset through this runtime
is affected: pools don't change status; their price-dependent operations fail
until the asset is untripped. USDC is a reserve of both Templar mainnet
pools, so tripping USDC affects both.

## Deploy

```sh
stellar contract deploy \
  --wasm target/wasm32v1-none/optimized/oracle_tripper.wasm \
  --network <NETWORK> \
  -- --governance <GOVERNANCE_CONTRACT_ID> \
     --caller <HYPERNATIVE_SYSTEM_ASSET_ADDRESS>
```

## Grant the role

The governance Admin (today a 2-of-3 smart account) grants `ManualTripper` to
the **tripper contract address** (not the Hypernative key) via a
`SetRole(<tripper>, ManualTripper, true)` proposal.

## Verify before relying on it

- `governance()` and `caller()` return the intended addresses.
- Governance `has_role(<tripper>, ManualTripper) == true`.
- Governance `get_operation_ttl(SetManualTrip) == 0`.
- A simulate-only `trip` (`--send no`) from the Hypernative key succeeds. This
  is mandatory before the role grant is relied on.
- On mainnet, the governance and runtime code must be the registry's
  proxy-governance v0.1.5 and proxy-oracle runtime v0.1.4 rows
  (`registry/mainnet.jsonl`); this crate's ABI is pinned to rev eea0060d of
  those releases.

## Hypernative action

- Chain: Stellar; action type: Single Contract Call; contract: the tripper;
  function: `trip`.
- One action per asset. Argument `asset` = the token contract address, and it
  must be one of the runtime's `registered_assets()` (as
  `{"Stellar": <address>}`); check it there rather than copying an example
  (mainnet USDC is `CCW67TSZV3SSS2HXMBQ5JFGCKJNXKZM7UQUWUZPUTHXSTZLEO7SJMI75`).
- The Hypernative system asset is the tx source/signer and must hold XLM for
  fees.

## Failure modes to monitor

- If another governance proposal lands between simulation and inclusion, the
  trip tx fails (footprint). Hypernative must retry / re-simulate.
- A ProxyConfigurationManager holder can raise `SetManualTrip`'s TTL
  (governance-common `required_role`), which breaks the tripper (create +
  execute no longer fit in one tx).
- Governance caps pending proposals at 64.
- A compromised `caller` can trip every Stellar feed repeatedly (no rate
  limit); recovery is untripping each asset.
- A failed `trip` surfaces raw error codes from both governance
  (`GovernanceError`: 2 Unauthorized, 5 ProposalNotMature, 8
  ProposalOutOfOrder, 9 InvalidInput) and the runtime (`ContractError`, e.g.
  4 InvalidInput for an unregistered asset) in one numeric space. Read the
  diagnostic events, not the number alone.

## Untrip (human)

Governance Admin (today the 2-of-3 smart account), via the multisig signing
flow:

1. `create_proposal(<admin>, <next_proposal_id>, SetManualTrip(Stellar(<asset>), false, None), 0)`
2. `execute_proposal(<admin>, <id>)`

## Rotation

No admin, setters or upgrade path. Revoke the old tripper first
(`SetRole(<old tripper>, ManualTripper, false)`), then deploy the new one and
grant it the role.

## Keep alive

`extend_ttl()` renews instance and code. Add the tripper to the keeper's
`extend_ttl_contracts` batch.
