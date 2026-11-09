# Fixtures

These two wasm files were fetched from mainnet with `stellar contract fetch`,
not built here. The tests register them with `env.register`, so the policy runs
against the deployed admin account's code rather than a local rebuild of its
source.

| File | Contract id | sha256 |
| --- | --- | --- |
| `admin_smart_account.wasm` | `CCDZKWLV6JWM4V3WPMJHXTSLWG5IDQ5S3K7BI3AH7MBCRQCPCT3EJ2AI` | `c514d35ea67eaea9bfc1fb3327854c0c06111bc1ddebd4fe217c504c1004b9e1` |
| `simple_threshold_policy.wasm` | `CCEW5A2TKD5CUB2MPSSA2QJW2HHTGJK62SM4FXSB7MSBIPGYLPIWADBJ` | `f72a11de6e2d5d2495d98fe61e04cd13544e64156a5a3be83ef8c2e403d14a94` |

`test.rs` pins both hashes. If either file changes, that test fails, and the
new file has to be checked by hand before the hash is updated.

Both are OpenZeppelin `stellar-contracts` v0.8.0, built with `soroban-sdk`
28.0.0, rustc 1.91.1 and stellar-cli 28.1.0 according to the `rsver`,
`rssdkver` and `cliver` entries in their `contractmetav0` section. The account
is the multisig `account` example, deployed as the Templar admin account. The
policy is the bundled `simple_threshold` policy, used on the pause rule so the
multisig tooling can read a threshold from it.

The metadata records no git commit. The attribution to OpenZeppelin commit
`ddc327eb`
(<https://github.com/OpenZeppelin/stellar-contracts/tree/ddc327eb>) rests on a
separate rebuild of that tag and on the crates.io `stellar-accounts` 0.8.0
source matching it.

Anything that deploys or verifies the policy must pin the wasm hash that
`release.yml` attests. That includes `PAUSE_POLICY_WASM` in the multisig tooling
(`tools/multisig/smart_account.py` in precept-internal) and the registry. A
local `stellar contract build` uses whatever toolchain is installed, which may
differ from the 1.98.0 that `rust-toolchain.toml` and CI pin, so its hash is not
a valid pin.

To re-fetch, run from `smart-account-policies/pool-pause-policy/`. Stellar CLI
28.1.0 takes the contract id only as `--id` or `--contract-id`, and the
passphrase is required because the CLI's default network is testnet:

```sh
stellar contract fetch \
  --id CCDZKWLV6JWM4V3WPMJHXTSLWG5IDQ5S3K7BI3AH7MBCRQCPCT3EJ2AI \
  --rpc-url https://mainnet.sorobanrpc.com \
  --network-passphrase 'Public Global Stellar Network ; September 2015' \
  -o testdata/admin_smart_account.wasm
stellar contract fetch \
  --id CCEW5A2TKD5CUB2MPSSA2QJW2HHTGJK62SM4FXSB7MSBIPGYLPIWADBJ \
  --rpc-url https://mainnet.sorobanrpc.com \
  --network-passphrase 'Public Global Stellar Network ; September 2015' \
  -o testdata/simple_threshold_policy.wasm
```
