# Official markets

**No official Precept market is live yet.** There is no mainnet deployment of Precept at the time of writing, and no
contract address on this page has been filled in. Until a market appears here, treat any Precept address you find
elsewhere — including one that looks plausible — as unverified.

## What a listing will contain

When a market is published, its entry will give everything needed to check what you are interacting with:

* the market's name and the pool, backstop and pool-factory contract addresses,
* the oracle address that the pool reads, and the assets it is configured to serve,
* for each asset: the underlying token address, its b-token and d-token addresses, and its decimals,
* the risk parameters actually in effect for each reserve — collateral factor, reserve factor, supply cap, and the
  interest-rate curve parameters,
* the pool's current status, and the release version and per-file checksums the deployed WASM was built from,
* the address that holds the pool's admin rights.

## Only trust addresses published here

Soroban addresses are opaque strings, and a Precept-shaped market can be deployed by anyone: a pool's name, its token
symbols and its user-facing text are chosen by whoever deployed it, and a pool factory only knows the backstop and
distributor it was constructed with. A contract address is the only thing that identifies a market, and it is only
meaningful when it comes from a source you trust.

**This page is the only place Precept contract addresses are published.** If a page, a post, or a wallet prompt shows
an address that does not appear here, do not send funds to it. Compare the address character by character rather than
by eye, and check the deployed WASM against the checksums published with the
[release](https://github.com/Templar-Protocol/precept/releases) before trusting a market with a deposit.
