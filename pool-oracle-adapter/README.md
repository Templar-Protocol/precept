# pool-oracle-adapter

A thin multi-asset SEP-40 price-oracle adapter over the Templar proxy oracle,
for use as a Blend v2 pool oracle.

- License: MIT (see `LICENSE`). The rest of this repository keeps its own licenses.
- Imported from Templar-Protocol/precept-internal@324fc5b
  (`contracts/pool-oracle-adapter`, the audited L-05 revision).
- Gate: `just audit-ready` (needs `stellar` CLI 28.1.0, Rust 1.98.0 with
  `wasm32v1-none`, and a `pool-adapter-env` wrapper on PATH; see the CI workflow).

## DANGEROUS: eager refresh build (`dangerous-eager-refresh`)

**Unaudited. Not the `v0.1.0` artifact. Do not deploy without a review.**

The default adapter is a pure view: `lastprice` reads the parent's cached
`aggregated_latest`, so a keeper must call the parent's `refresh` to keep
prices fresh. Built with `--features dangerous-eager-refresh`, `lastprice`
first calls the parent's permissionless `refresh(asset)`, ignores the result,
then reads the cache as usual. Every priced pool action (borrow, a
health-checked withdrawal, liquidation, write-off) pulls its own price, so a
market needs no keeper for freshness. `price`, `prices`, the ABI and every
other entry point are unchanged.

What still holds: the parent's sources, median, freshness limits and breakers
decide every price, and a blocked or failed refresh makes `lastprice` return
`None`.

Why it is dangerous:

- **Reads write, but a pool keeps only the successes.** A `lastprice` call
  inside a transaction rewrites the parent's cache and breaker state. A pool
  traps when it gets `None`, which reverts the whole transaction, so a blocked
  or failed refresh made through a pool never persists: the cache keeps its
  last accepted price and a breaker that tripped does not stay tripped. Every
  later action re-runs the refresh and fails again while the cause lasts, and
  the trip clears by itself once sources recover. Only a direct `refresh` call
  (a keeper, or anyone) persists a failure. Breakers also sample only when an
  action succeeds, so rate-of-change breakers see sparse, demand-driven history.
- **Outages bite at once.** A still fresh cached price no longer covers a
  source outage: each priced action refreshes, fails, and reverts, instead of
  being served from the cache until the next keeper run or `max_age_secs`. A
  source whose ledger entries are archived traps below the parent's `try_`
  calls with the same result.
- **Users pay for it.** Each priced reserve costs a refresh (three source views
  per source plus a cache write) inside the user's transaction, up to
  `max_positions` of them. Size source sets with the transaction budget in mind.
- **Freshness is only as good as the sources' own push.** With no keeper, a
  price is as old as the source's last update, so `max_age_secs` must cover
  the source heartbeat.
- **TTL is still yours.** Nothing here extends the parent's or the sources'
  ledger entries; keep the daily `extend_ttl` job.

Gate: `just eager-ready` (clippy, tests and build with the feature, the same
frozen ABI check and size cap). It builds into `target/eager`, so it never
touches the audited artifact or its pin.
