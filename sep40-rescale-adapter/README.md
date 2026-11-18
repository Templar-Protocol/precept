# sep40-rescale-adapter

**LOSSY, unaudited. Do not deploy without a review.**

A SEP-40 oracle that forwards every read, live, to another SEP-40 oracle and
serves the answer at 7 decimals, the precision a Precept pool requires. It
caches nothing, so no keeper refreshes it: a price is exactly as fresh as the
source's own last update.

Use it to point a pool straight at a third-party feed, such as RedStone's
SEP-40 wrapper (8 decimals), with no proxy runtime in between.

## Behaviour

- Constructor: `(source: Address, aliases: Map<Asset, Asset>)`, both
  immutable. No admin, no upgrade; a new source or alias means a new deploy.
- `aliases` maps the asset a caller asks for to the key the source serves it
  under. A pool always asks for `Stellar(<reserve token>)`; RedStone serves
  earnUSDC as `Other("earnUSDC_PPS")`, so that pair goes in the map. Unlisted
  assets are forwarded as-is.
- `decimals()` is always 7. `lastprice`, `price` and `prices` read the
  source's `decimals()` on every call and rescale:
  - **down** (source has more than 7): integer division, truncating toward
    zero. A price loses up to one unit of the 7th decimal; RedStone's
    `102020197` (8 dp) becomes `10202019`.
  - **up** (source has fewer than 7): exact multiplication.
  - overflow, a scale beyond `10^38`, or a result that is not positive returns
    `None`. `prices` returns `None` if any record cannot be represented.
- A source error or trap on a price read returns `None`. `base`, `assets` and
  `resolution` are forwarded unchanged and trap if the source traps.
- Reads extend the adapter's instance TTL; `extend_ttl()` does the same and is
  permissionless.

## Why it is dangerous

- **No safety layer.** There are no breakers, no median and no age limit
  beyond the pool's own 24 hours and future-timestamp checks. A wrong or stale
  source price goes straight to the pool.
- **Truncation is one-sided.** Every scaled-down price is rounded toward zero,
  so collateral and debt are both valued up to 1e-7 of the base unit low.
- **The source owns the decimals.** RedStone documents that its wrapper's
  decimals can change when feeds are added. The adapter follows them on every
  read, but a consumer that cached an earlier price sees a scale jump.
- **The source owns liveness.** If it goes quiet, prices age out of the pool's
  24-hour window and priced actions stop. If its ledger entries are archived,
  reads trap.
- **Aliases are fixed.** If the source renames a feed, as RedStone did for
  earnUSDC (`earnUSDC_FUNDAMENTAL/USD` → `earnUSDC_PPS_FUNDAMENTAL/USD`), the
  adapter keeps asking for the old key until it is redeployed.

## Build and test

```bash
cargo test -p sep40-rescale-adapter
just build   # writes target/wasm32v1-none/optimized/sep40_rescale_adapter.wasm
```

It is not part of the release artifacts and no market in the registry uses it.
