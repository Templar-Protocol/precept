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

Both are OpenZeppelin `stellar-contracts` **v0.8.0** (commit `ddc327eb`), built
with `soroban-sdk` 28.0.0 / `rs` 1.91.1 per their embedded
`contractenvmetav0` metadata. The account is the multisig `account` example
deployed as the Templar admin account; the policy is the bundled
`simple_threshold` policy, used as the co-policy that makes a single pauser
signature sufficient.

Source: <https://github.com/OpenZeppelin/stellar-contracts/tree/ddc327eb>

To re-fetch:

```sh
stellar contract fetch \
  --network network \
  --rpc-url https://mainnet.rpc.stellar.xyz \
  CCDZKWLV6JWM4V3WPMJHXTSLWG5IDQ5S3K7BI3AH7MBCRQCPCT3EJ2AI \
  -o testdata/admin_smart_account.wasm
stellar contract fetch \
  --network network \
  --rpc-url https://mainnet.rpc.stellar.xyz \
  CCEW5A2TKD5CUB2MPSSA2QJW2HHTGJK62SM4FXSB7MSBIPGYLPIWADBJ \
  -o testdata/simple_threshold_policy.wasm
```
