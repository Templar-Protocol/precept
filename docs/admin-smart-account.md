# Mainnet admin smart account (2-of-2)

The mainnet admin is an OpenZeppelin multisig smart account: a contract that
holds authority and enforces 2-of-2 approval through a threshold policy
contract. No single key can act as admin.

This page records what is deployed and how it behaves. The machine-readable
rows live in [`registry/mainnet.jsonl`](../registry/mainnet.jsonl); run
`python3 registry/registry.py check` to validate them.

## Deployed ids

| What | Id | Role in the registry |
| --- | --- | --- |
| Multisig smart account | `CCDZKWLV6JWM4V3WPMJHXTSLWG5IDQ5S3K7BI3AH7MBCRQCPCT3EJ2AI` | `admin_smart_account` |
| Simple threshold policy | `CCEW5A2TKD5CUB2MPSSA2QJW2HHTGJK62SM4FXSB7MSBIPGYLPIWADBJ` | `admin_policy` |

Both were created on 2026-10-27, the policy first:

- policy create tx `ba89b324832899d40df0a91c099987cf6204ae78f441fe8ec86fc6d3b0cd6d6f`
  (ledger 65165774)
- account create tx `be6e5c24549c101a169df16b91adcc112755296d7b3ea9dc3d1250eb98f83aeb`
  (ledger 65165779)

## Code

| Contract | Wasm hash | Bytes |
| --- | --- | --- |
| Multisig smart account | `c514d35ea67eaea9bfc1fb3327854c0c06111bc1ddebd4fe217c504c1004b9e1` | 48273 |
| Simple threshold policy | `f72a11de6e2d5d2495d98fe61e04cd13544e64156a5a3be83ef8c2e403d14a94` | 13697 |

Provenance, both contracts:

- Source: [OpenZeppelin/stellar-contracts](https://github.com/OpenZeppelin/stellar-contracts)
  v0.8.0, commit `ddc327eb92d63709436da3a8455f641c2d5476c5` — the
  `multisig-smart-account` account and `threshold-policy` examples.
- Uploaded to mainnet by the Stellar Development Foundation, not by Templar,
  so the deployed bytes are the SDF build rather than a Templar release asset.
- Reproduced from that tree with Rust 1.91.1 and Stellar CLI 28.1.0. The
  policy rebuilds byte-identical. The account rebuild differs from the
  deployed bytes only in function order (same code, different code-section
  layout), so its hash cannot be reproduced locally; deploy instances with
  `--wasm-hash c514d35e…` instead of the locally built wasm.

## Signers and threshold

Two signers, threshold 2 — both must sign every admin action:

- `GAY47MJUC7TKS76Q2X2HUNBIOHX6WO6LCFA2A424KQV723EU5TDQG44M`
- `GBJFHU35WHKY24GGBY6R4PTEHLMURSQEVTP2TUX5O45XYGG5DI6YS2QV`

The threshold lives in the policy contract, per context rule; the signer set
lives in the account. The account's single default context rule (named
`multisig`) wires the two together.

## What the account can and cannot do

- Every state change the account controls — adding or removing signers,
  changing the threshold, upgrading the account's own code, and any admin call
  the account makes on a pool — requires the account's own authorization,
  which means 2-of-2 signatures.
- The policy is only mutable through the account: `set_threshold` requires
  authorization from the smart account, so changing the threshold is itself a
  2-of-2 action.
- Signer and threshold changes are independent. Adding a signer does **not**
  change the threshold (it stays 2), and removing a signer does not lower the
  threshold. Removing a signer while the threshold is 2 leaves one signer
  against a threshold of 2, and the account can no longer authorize anything.
  To remove a signer from a 2-of-2: add the new signer first, then
  `set_threshold` to 1 while everyone is still able to sign, then remove the
  old signer, then `set_threshold` back to 2.
- The threshold may never be 0 or exceed the number of signers, so keep the
  signer count at or above the threshold at all times.
- The account is upgradeable, and the upgrade is authorized the same way as
  everything else (2-of-2).

## Acting as admin

- Tooling: `tools/multisig` in the precept-internal repository builds the
  signer payloads, collects the two signatures and submits the call.
- Hosted signing page for signers:
  <https://templar-protocol.github.io/precept/sign/>.
- Anything that expects an admin address takes the account id
  `CCDZKWLV6JWM4V3WPMJHXTSLWG5IDQ5S3K7BI3AH7MBCRQCPCT3EJ2AI`, not either
  signer's `G…` address.

## Hand-off status

**Pending — pools are still administered by the deploy key until hand-off.**
The registry's pool rows therefore still record the deploy account in their
`admin` field. The smart account is deployed and configured; it does not yet
control the pools.
