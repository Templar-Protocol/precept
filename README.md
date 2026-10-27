# Precept

Precept is Templar's fork of [Blend v2](https://github.com/blend-capital/blend-contracts-v2) for permissionless
lending pools on Soroban. A pool keeps Blend's mechanics — b-dollars and d-dollars, interest accrued on demand
through utilization curves, liquidation auctions filled against live pool state — and removes the parts that would
route a loss somewhere other than the people who lent the money. The backstop's insurance and rewards are switched
off (`distribute`, `gulp_emissions`, `add_reward`, `remove_reward`, `claim`, `drop`), the interest and bad-debt
auctions are switched off, flash loans are switched off, emissions cannot be configured, and the pool's own
`update_pool` entrypoint is switched off. The backstop's share of interest is fixed at 0 when a pool is deployed, so
there is no protocol fee and the backstop is never paid out of reserve funds. What is left is a straightforward
contract: interest goes to the lenders of the reserve that was lent, and so does any loss — socialised pro rata to
that reserve's b-dollar holders, in the same transaction that recognises it.

There is no official Precept market live yet, and the pool, backstop and factory contracts have no upgrade path: a
live market's behaviour is fixed by the release it was deployed from.

## Documentation

The public documentation is maintained in this repository under [`docs/`](docs/README.md):

- [How Precept works](docs/how-it-works.md) — the flow from an under-collateralised position to a socialised loss.
- [Lending economics](docs/lending-economics.md) — what a lender is exposed to, and how the oracle and interest rate
  are constructed.
- [Risks](docs/risks.md) — what can go wrong and every admin or governance lever that affects a market.
- [Changes from Blend v2](docs/changes-from-blend-v2.md) — a stock-versus-Precept table with a permalink to the code
  on both sides of every row.
- [Official markets](docs/official-markets.md) — which markets are official, and the addresses to trust.
- [Security and audits](docs/security.md) — reviews, reproducible builds, and how to report a vulnerability.

For the mechanics this fork inherits unchanged, read the upstream
[Blend documentation](https://docs.blend.capital/) rather than a second copy of the same math.

## Audits

Guardian reviewed this fork; the report is in [`audits/`](audits/Guardian%20-%20Templar%20-%20Blend%20Contracts%20V2%20Security%20Review%20-%20October%202026.pdf)
alongside the upstream Blend v2 audits that cover the stock code this fork derives from
([Code4rena](audits/Code4rena%20x%20Blend%20V2%20audit%20report.pdf),
[Certora formal verification](audits/Script3%20-%20Certora%20-%20Blend%20v2%20-%20Formal%20Verification%20Draft%20v2%20Report%20-%20June%202025.pdf),
[Certora security assessment](audits/Script3%20-%20Certora%20-%20Blend%20v2%20-%20Security%20Assessment%20Draft%20v3%20Report%20-%20April%202025.pdf)).
[Security and audits](docs/security.md) summarises the findings and their status.

## Formal verification

[`VERIFICATION.md`](VERIFICATION.md) gives the status of the seven safety
invariants, what each one means, and the assumptions it is proven under.

## Getting Started

Build the contracts with:

```
make
```

Run all unit tests and the integration test suite with:

```
make test
```

Explicit base-versus-fork differential (ADR 0008 / ADR 0011; builds the pinned stock baseline from git). Run inside `devenv shell`, which provides the pinned Stellar CLI:

```
make differential
```

## Deployment

The `make` command creates an optimized and un-optimized set of WASM contracts. It's recommended to use the optimized version if deploying to a network.

These can be found at the path:

```
target/wasm32v1-none/optimized
```

The `pool-oracle-adapter` builds separately: `cd pool-oracle-adapter && just preflight && just optimize`.

Released builds are published on the
[releases page](https://github.com/Templar-Protocol/precept/releases) with the four artifacts and a `SHA256SUMS` file
per release; the release workflow rebuilds the optimized WASMs from the tagged source and fails if any rebuilt file
fails to match the published checksums. See [Security and audits](docs/security.md#reproducing-a-release-build).

## Contributing

Notes for contributors:

- Under no circumstances should the "overflow-checks" flag be removed otherwise contract math will become unsafe

## License

Precept is licensed under the MIT License — see [LICENSE](LICENSE). It is a fork of
[Blend v2](https://github.com/blend-capital/blend-contracts-v2), which Blend relicensed from AGPL-3.0 to MIT in
[`f0331f17`](https://github.com/blend-capital/blend-contracts-v2/commit/f0331f178484f55e7654e8e35707e39f1eecf042);
Blend's copyright notice is kept in [LICENSE](LICENSE). The `pool-oracle-adapter` crate is also MIT — see
[pool-oracle-adapter/LICENSE](pool-oracle-adapter/LICENSE). The prebuilt `comet.wasm` used only by the tests is a
binary of [Comet](https://github.com/CometDEX/comet-contracts-v1) under its own license (AGPL-3.0) and is not covered
by LICENSE.

## Security

Report vulnerabilities to **security@templarprotocol.com**. Precept does not currently operate a bug bounty programme.

## Upstream

Precept is a fork of Blend v2 by [Blend Capital](https://github.com/blend-capital). The upstream implementation is at
[blend-capital/blend-contracts-v2](https://github.com/blend-capital/blend-contracts-v2), and its documentation at
[docs.blend.capital](https://docs.blend.capital/).
