# Fixtures

Wasm pulled from the **public network** with `stellar contract fetch`, not built
here. They are the binaries the tests register with `env.register`, so the
policy is exercised against the deployed mainnet account rather than a local
rebuild of its source.

| File | Contract id | sha256 |
| --- | --- | --- |
| `admin_smart_account.wasm` | `CCDZKWLV6JWM4V3WPMJHXTSLWG5IDQ5S3K7BI3AH7MBCRQCPCT3EJ2AI` | `c514d35ea67eaea9bfc1fb3327854c0c06111bc1ddebd4fe217c504c1004b9e1` |
| `simple_threshold_policy.wasm` | `CCEW5A2TKD5CUB2MPSSA2QJW2HHTGJK62SM4FXSB7MSBIPGYLPIWADBJ` | `f72a11de6e2d5d2495d98fe61e04cd13544e64156a5a3be83ef8c2e403d14a94` |

`test.rs` pins both hashes; if either file changes, the pin test fails and the
fixture has to be re-verified by hand before the hash is updated.

Both are OpenZeppelin `stellar-contracts` **v0.8.0**, built with `soroban-sdk`
28.0.0 / rustc 1.91.1 / stellar-cli 28.1.0 per the `contractmetav0` section
they embed (`rsver`, `rssdkver`, `cliver`). The account is the multisig
`account` example deployed as the Templar admin account; the policy is the
bundled `simple_threshold` policy, used as the co-policy that lets the multisig
tooling read a threshold off the pause rule.

No git commit is recorded in that metadata, so the source attribution to commit
`ddc327eb` does not come from these files: it rests on W2.1's rebuild of that
tag (outside this PR) and on the crates.io `stellar-accounts` 0.8.0 source
matching it. Source:
<https://github.com/OpenZeppelin/stellar-contracts/tree/ddc327eb>.

Pin the **CI release** wasm hash for anything that deploys. A local
`stellar contract build` hashes whatever toolchain happens to be installed
(here rustc 1.99, while `rust-toolchain.toml` and CI pin 1.98.0), so its hash
is not a pin: only the hash `release.yml` attests is the one
`PAUSE_POLICY_WASM` and the registry may use. The `adf30835…` in the PR body is
such a local build, recorded for reference only.

To re-fetch (from `smart-account-policies/pool-pause-policy/`; `stellar-cli`
28.1.0 takes the contract id only as `--id`/`--contract-id`, and the
passphrase is required because the default network is testnet):

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
