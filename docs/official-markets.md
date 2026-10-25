# Official markets

**No official Precept market is live yet.** There is no mainnet deployment of Precept at the time of writing, and no
contract address on this page has been filled in. Until a market appears here, treat any Precept address you find
elsewhere — including one that looks plausible — as unverified.

## What a listing will contain

When a market is published, its entry will give everything needed to check what you are interacting with:

* the market's name and the pool, backstop and pool-factory contract addresses,
* the oracle address that the pool reads, and the assets it is configured to serve,
* for each asset: the underlying token address and its decimals,
* the risk parameters actually in effect for each reserve — `c_factor`, `l_factor`, `util`, `max_util`, the
  interest-curve coefficients `r_base`, `r_one`, `r_two` and `r_three`, `reactivity`, `supply_cap`, `min_position` and
  `enabled` ([pool/src/storage.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/storage.rs#L58-L73)
  — and the proxy oracle's `max_age_secs` for that asset, the age beyond which the feed stops returning a price
  ([what the pool checks](lending-economics.md#what-the-pool-checks)),
* the pool's current status, and the release version and per-file checksums the deployed WASM was built from,
* the address that holds the pool's admin rights.

## Only trust addresses published here

Soroban addresses are opaque strings, and a Precept-shaped market can be deployed by anyone: a pool's name, its token
symbols and its user-facing text are chosen by whoever deployed it, and a pool factory only knows the backstop and
BLND token addresses it was constructed with. A contract address is the only thing that identifies a market, and it is
only meaningful when it comes from a source you trust
([pool-factory/src/storage.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool-factory/src/storage.rs#L19-L25)).

**The official mainnet markets are the ones listed on this page.** The repository also keeps a
[deployment registry](https://github.com/Templar-Protocol/precept/tree/main/registry) of every contract Templar has
deployed, including testnet and historical deployments; it records what exists, not what is official. If a page, a post,
or a wallet prompt shows
an address that does not appear here, do not send funds to it. Compare the address character by character rather than
by eye, and check the deployed WASM against the checksums published with the
[release](https://github.com/Templar-Protocol/precept/releases) before trusting a market with a deposit.
