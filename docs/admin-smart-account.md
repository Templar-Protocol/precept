# Mainnet admin smart account (2-of-3)

The mainnet admin is an OpenZeppelin multisig smart account: a contract that
holds authority and enforces 2-of-3 approval through a threshold policy
contract. No single key can act as admin.

This page records what is deployed and how it behaves. The machine-readable
rows live in `registry/mainnet.jsonl`; run
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

Three signers, threshold 2 — any two must sign every admin action (2-of-3):

- `GAY47MJUC7TKS76Q2X2HUNBIOHX6WO6LCFA2A424KQV723EU5TDQG44M`
- `GBJFHU35WHKY24GGBY6R4PTEHLMURSQEVTP2TUX5O45XYGG5DI6YS2QV`
- `GA7QWQU4TP4UNOSRZEKN3XRVGSZI6OTPZRULTSJ5OSXEA4WEW4VOTKGM`

The account was created 2-of-2 with the first two signers. On 2026-10-27 the
third signer was added with `add_signer`, tx
`cdb354317a4639a3e2b232406edf6488472089946bdc52dcc5acda0dc399c22b` (ledger
65167927, submitted by the deploy key), and the threshold policy stayed at 2.
As a negative check, attempts authorized by only one signer were rejected with
contract error `#3202`.

The threshold lives in the policy contract, per context rule; the signer set
lives in the account. The account's single default context rule (named
`multisig`) wires the two together.

## What the account can and cannot do

- Every state change the account controls — adding or removing signers,
  changing the threshold, upgrading the account's own code, and any admin call
  the account makes on a pool — requires the account's own authorization,
  which means two signatures from the three signers.
- The policy is only mutable through the account: `set_threshold` requires
  authorization from the smart account, so changing the threshold is itself a
  two-signature action.
- Signer and threshold changes are independent. Adding a signer does **not**
  change the threshold (it stays 2), and removing a signer does not lower the
  threshold. The account can only authorize while the signer count is at or
  above the threshold, so never remove a signer if that would leave fewer
  signers than the threshold; lower the threshold first with `set_threshold`
  while enough signers can still sign.
- The threshold may never be 0 or exceed the number of signers.
- The account is upgradeable, and the upgrade is authorized the same way as
  everything else (two of the three signers).

## Acting as admin

- Tooling: `tools/multisig` in the precept-internal repository builds the
  signer payloads, collects two signatures and submits the call.
- Hosted signing page for signers:
  <https://templar-protocol.github.io/precept/sign/>.
- Anything that expects an admin address takes the account id
  `CCDZKWLV6JWM4V3WPMJHXTSLWG5IDQ5S3K7BI3AH7MBCRQCPCT3EJ2AI`, not any
  signer's `G…` address.

## Hand-off status

**Complete (2026-10-27).** The deploy key `GD4S5HCG…RHKY` holds no authority; read back from chain:

| Item | Holder | Accepting tx |
|---|---|---|
| xlm-usdc pool `CAILFZ2M…7DPF` admin | account | `911ffe19…b4b9` (`accept_admin`) |
| dejaaa-usdc pool `CBTC5WFK…QULS` admin | account | `4ebd8428…b42c` (`accept_admin`) |
| Lazer source `CDXOHZDL…E5IB` owner | account | `4eac8afa…344a` (`accept_ownership`) |
| Governance `CD6AR7CH…QTI4` Admin | account only | grant by the deploy key; its own Admin revoked by the account, `53ff857d…84ed` (`execute_proposal(4)`) |
| Comet controllers `CBGHPG24…TKPM`, `CC7KUL3L…UTBW` | account | one-step `set_controller` by the deploy key, `85e35c2c…`, `31b37b40…` |
| Proxy runtime `CBTCHY4L…GI4X` owner | governance (by design) | — |

Governance roles `ManualTripper`, `CircuitBreakerOperator` and `ProxyConfigurationManager`
are unassigned, so every governance action needs two of the three signers.

Both pools were then activated by the account (`set_status(0)`, Admin Active):
xlm-usdc `708abf30…5390`, dejaaa-usdc `d4f517ef…2484`.
