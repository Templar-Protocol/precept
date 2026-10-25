# Changes from Blend v2

Precept is a fork of [Blend v2](https://github.com/blend-capital/blend-contracts-v2). This page lists every way the
two contracts behave differently, with a permalink into the code on both sides of each row. If something is not listed
here, it is the same code.

| Side | Commit |
| --- | --- |
| Precept | `62c8e9bee072d5e58494c0f7a7bf777926e7ea2b` (`main`) |
| Stock Blend v2 | `6cf6dd1ca712ea3cf17a767fe49b7da01f80b909` |

The code released as v0.1.1 matches that commit: `git diff` between the tag and `main` touches only proof harnesses,
fuzz targets, build files and test comments — no contract source. v0.1.0 is the earlier, Guardian-remediated code; the
release notes for each version describe what changed between them, including the event-topic changes noted below.

The pattern of the fork is consistent: the code paths a loss would normally travel through are kept, because other
paths call them, and the entrypoints that would let anyone walk into them are switched off. The pool still references
a backstop, still keeps an auction book, and still reads a price feed. The backstop, the auctions and the price feed
just no longer do anything that moves value.

## Disabled pool entrypoints

| Entrypoint | Stock behaviour | Precept behaviour |
| --- | --- | --- |
| `update_pool` | Admin updates the backstop take rate, the position limit and the minimum collateral ([pool/src/contract.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/contract.rs#L392-L400)) | Traps with `PoolError::BadRequest` (1200) ([pool/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/contract.rs#L406-L408)) |
| `flash_loan` | A 0-fee same-block borrow and repay ([pool/src/contract.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/contract.rs#L484-L494)) | Traps with `PoolError::BadRequest` (1200) ([pool/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/contract.rs#L492-L499)) |
| `set_emissions_config` | Admin configures reserve emissions ([pool/src/contract.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/contract.rs#L530-L537)) | Traps with `PoolError::BadRequest` (1200) ([pool/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/contract.rs#L536-L538)) |

`update_status`, `gulp`, `bad_debt`, `new_auction` and `del_auction` need no admin signature and are not trapped, and
`submit` is the ordinary user entrypoint. `claim` is likewise not trapped, but has nothing to pay because no emissions
can be configured, and `gulp` can no longer move value (see below).

## Disabled backstop entrypoints

Every one of these traps with `BackstopError::BadRequest` (1000). The stock column cites the same functions in the
stock contract, where they are live.

| Entrypoint | Stock behaviour | Precept |
| --- | --- | --- |
| `distribute` | Pays accrued interest out of the backstop's credit to a pool ([backstop/src/contract.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/backstop/src/contract.rs#L262-L266)) | [backstop/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/backstop/src/contract.rs#L262-L264) |
| `gulp_emissions` | Updates the backstop's reward emissions ([backstop/src/contract.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/backstop/src/contract.rs#L268-L272)) | [backstop/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/backstop/src/contract.rs#L266-L268) |
| `add_reward` | Admin adds (and may replace) a reward token ([backstop/src/contract.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/backstop/src/contract.rs#L274-L284)) | [backstop/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/backstop/src/contract.rs#L270-L272) |
| `remove_reward` | Admin removes a reward token ([backstop/src/contract.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/backstop/src/contract.rs#L286-L292)) | [backstop/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/backstop/src/contract.rs#L274-L276) |
| `claim` | Claims the backstop's reward emissions ([backstop/src/contract.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/backstop/src/contract.rs#L302-L306)) | [backstop/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/backstop/src/contract.rs#L278-L280) |
| `drop` | Drops a reward token's emissions ([backstop/src/contract.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/backstop/src/contract.rs#L308-L312)) | [backstop/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/backstop/src/contract.rs#L282-L284) |

The backstop's own deposits, withdrawals, donations and draws are unchanged: the backstop is still a functioning
token-pool contract, it just has no role in settling a loss.

## Auctions: user liquidation only

| Behaviour | Stock | Precept |
| --- | --- | --- |
| Create a bad-debt (1) or interest (2) auction | Creates it ([pool/src/auctions/auction.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/auctions/auction.rs#L99-L123)) | Traps with `PoolError::BadRequest` (1200) ([pool/src/auctions/auction.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/auctions/auction.rs#L109-L111)) |
| Fill a bad-debt or interest auction | Runs the corresponding fill ([pool/src/auctions/auction.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/auctions/auction.rs#L175-L213)) | Traps with `PoolError::BadRequest` (1200) ([pool/src/auctions/auction.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/auctions/auction.rs#L184-L186)) |
| Fill request types 1 / 2 through `submit` | Reaches the corresponding fill ([pool/src/pool/actions.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/actions.rs#L271-L281)) | Reaches the same dispatch and traps inside `fill` ([pool/src/pool/actions.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L273-L312)) |
| Deleting a stale auction, deleting your own liquidation | Deleting an auction type with no live auction traps with `BadRequest` | Same: these two paths are not switched off ([pool/src/auctions/auction.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/auctions/auction.rs#L126-L172)) |

The consequence is structural, not cosmetic: with no interest auction, accrued interest is not diverted to the backstop
to be sold, and with no bad-debt auction, a shortfall is never converted into a sale of pool assets. Both of those
paths led to the backstop, and Precept removes them rather than leaving them pointing at a backstop that cannot pay.
The stock code that would have run them is still present but unreachable.

## Bad debt: setoff, then the reserve's lenders, then orphans

Stock routes a user's bad debt to the backstop. Precept has no backstop to route it to, so the settlement order inside
`bad_debt` is different, and it emits events that stock does not.

| Behaviour | Stock | Precept |
| --- | --- | --- |
| Where residual debt goes | Increased backstop bad debt for the pool, which then creates a bad-debt auction, with a direct-default fallback for the rare case that cannot fund ([pool/src/pool/bad_debt.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/bad_debt.rs#L53-L78)) | Set off against the user's own supply in that reserve, then defaulted against that reserve's b-dollar rate ([pool/src/pool/bad_debt.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/bad_debt.rs#L54-L118)) |
| Who absorbs it | The backstop's backers, through its credit and token balance | The b-token holders of the reserve that was lent, pro rata ([pool/src/pool/user.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/user.rs#L110-L160)) |
| Residual collateral after a default | Stays with the defaulted user | Recorded under a pool-owned user position, so it no longer belongs to the defaulted account ([pool/src/pool/bad_debt.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/bad_debt.rs#L104-L112)) |
| The backstop as a defaulting party | A backstop default is a supported path ([pool/src/pool/bad_debt.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/bad_debt.rs#L6-L32)) | Rejected: the entrypoint refuses the backstop address ([pool/src/pool/bad_debt.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/bad_debt.rs#L17-L19)) |

A pool-owned (orphaned) position stays in the reserve's b-token supply until the reserve carries no debt at all, at
which point `gulp` burns it
([pool/src/pool/gulp.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/gulp.rs#L11-L30)).

### Events, with their current topics

| Event | Topics | Data | Reference |
| --- | --- | --- | --- |
| `debt_setoff` | `name, user, asset` | b-tokens burned and d-tokens repaid | [pool/src/events.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/events.rs#L188-L198) |
| `defaulted_debt` | `name, user, asset` | defaulted d-token amount | [pool/src/events.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/events.rs#L179-L182) |
| `collateral_orphaned` | `name, user, asset` | orphaned b-token amount | [pool/src/events.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/events.rs#L201-L204) |
| `orphan_settled` | `name, asset` | burned pool-owned b-token amount | [pool/src/events.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/events.rs#L207-L210) |

`debt_setoff`, `collateral_orphaned` and `orphan_settled` do not exist in the stock event surface at all
([pool/src/events.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/events.rs#L155-L174)),
and stock's `defaulted_debt` carries only `[name, asset]`. In this repository `defaulted_debt` and `debt_setoff`
gained the defaulted user as a topic after the first release, so indexers should read the topic layout from the release
they are decoding: the layout above is the current one. Carrying the defaulted user is what lets a listener attribute
each socialised loss to the position that caused it without decoding the event data. Stock's `bad_debt` event remains
defined here but is no longer emitted by any code path.

## Interest and the backstop's share

| Behaviour | Stock | Precept |
| --- | --- | --- |
| `bstop_rate` at initialisation | Any value below 1.0 is accepted ([pool/src/pool/config.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/config.rs#L16-L42)) | Must be 0 ([pool/src/pool/config.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/config.rs#L27-L29); factory check: [pool-factory/src/pool_factory.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool-factory/src/pool_factory.rs#L75-L78)) |
| `gulp` | Any reserve funds above the accounting expectation are accrued to the reserve's backstop credit and returned as the delta ([pool/src/contract.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/contract.rs#L513-L519), [pool/src/pool/gulp.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/gulp.rs#L18-L37)) | Returns 0 and accrues nothing to the backstop; it only burns pool-owned positions once a reserve carries no debt ([pool/src/pool/gulp.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/gulp.rs#L11-L30)) |

Interest accrual, the utilization curve and the reserve exchange rates are otherwise the same code, so the interest a
borrower pays reaches the reserve's lenders through the b-dollar rate rather than being split with the backstop.

## Pool and reserve configuration

| Behaviour | Stock | Precept |
| --- | --- | --- |
| Reserve configuration after activation | A queued reserve change lands with whatever configuration was queued, for a new or an existing reserve ([pool/src/pool/config.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/config.rs#L98-L111)) | Only `enabled: true → false` is accepted for an existing reserve; every other field must be identical ([pool/src/pool/config.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/config.rs#L115-L134), [pool/src/pool/config.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/config.rs#L211-L227)) |
| Adding a reserve after activation | Possible through the normal queue and timelock | Not possible: queueing a change to a market that is already live reverts unless it is a disable, and a change that would introduce an unknown asset is rejected when it lands ([pool/src/pool/config.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/config.rs#L85-L88), [pool/src/pool/config.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/config.rs#L123-L128)) |
| Supply cap | Enforced when minting b-dollars on the supply and supply-collateral paths, and editable until the pool is activated ([pool/src/pool/actions.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/actions.rs#L300-L302), [pool/src/pool/actions.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/actions.rs#L336-L342)) | The same checks are unchanged; the cap is never editable afterwards, because it is one of the fields the disable-only rule freezes ([pool/src/pool/actions.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L349-L351), [pool/src/pool/actions.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L452-L454)) |
| Per-reserve minimum position | Not present | A reserve carries a `min_position` value; supplying or withdrawing collateral that leaves the reserve holding less than it reverts with `MinCollateralNotMet` (1224) ([pool/src/storage.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/storage.rs#L71), [pool/src/pool/actions.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L417-L423)) |
| Admin handover | `propose_admin` stores a proposal with no expiry | Same two-step flow, but the proposal is stored under a temporary entry that lapses after ten days ([pool/src/storage.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/storage.rs#L290-L320)) |

## Status rules

| Behaviour | Stock | Precept |
| --- | --- | --- |
| Setting status 4 (admin freeze) | Sets it; another `set_status` call can leave it ([pool/src/pool/status.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/status.rs#L71-L110)) | Any `set_status` to anything other than 4 reverts with `StatusNotAllowed`, including by the admin ([pool/src/pool/status.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/status.rs#L84-L141)) |
| Automatic status updates while frozen | Recompute normally | `update_status` reverts while the pool is on 4 ([pool/src/pool/status.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/status.rs#L16-L20)) |
| Leaving `setup` | No oracle format requirement ([pool/src/pool/status.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/status.rs#L68-L76)) | The configured oracle must report 7 decimals, or `set_status` reverts with `InvalidPrice` ([pool/src/pool/status.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/status.rs#L95-L101)) |

The numeric statuses, their meanings and the automatic transitions driven by the backstop's balance are otherwise
unchanged.

## Price checks

`load_price` accepts a price only if all of the following hold; any failure traps with `PoolError::InvalidPrice`
([pool/src/pool/pool.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/pool.rs#L123-L143)):

| Check | Stock | Precept |
| --- | --- | --- |
| The feed does not answer | The absent price is unwrapped and panics rather than returning an error ([pool/src/pool/pool.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/pool.rs#L100-L130)) | Traps with `PoolError::InvalidPrice` |
| `last_updated` is dated ahead of ledger time | Not checked ([pool/src/pool/pool.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/pool.rs#L100-L130)) | Traps with `PoolError::InvalidPrice` |
| `last_updated` is older than 24 hours | Traps with `InvalidPrice` ([pool/src/pool/pool.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/pool.rs#L100-L130)) | Unchanged |
| Price is zero or negative | Traps with `InvalidPrice` | Unchanged |
| The feed's `decimals()` is not 7 | Not checked ([pool/src/pool/pool.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/pool.rs#L100-L113)) | Traps with `InvalidPrice` ([pool/src/pool/pool.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/pool.rs#L100-L113)) |

Prices are read only for reserves where the account actually holds a balance, and not at all by a `bad_debt` call that
has no collateral left to value
([pool/src/pool/bad_debt.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/bad_debt.rs#L63-L74)).

## Emissions

| Behaviour | Stock | Precept |
| --- | --- | --- |
| Configuring reserve emissions | Admin call ([pool/src/contract.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/contract.rs#L530-L537)) | Trapped, so no reserve can ever carry an emissions schedule ([pool/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/contract.rs#L536-L538)) |
| `claim` / `get_reserve_emissions` / `get_user_emissions` | Live | Present but inert — with no configuration they have nothing to pay out ([pool/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/contract.rs#L540-L561)) |
| `gulp_emissions` (pool → backstop) | Live | Reaches the backstop's trapped `gulp_emissions`, so it cannot succeed ([backstop/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/backstop/src/contract.rs#L266-L268)) |

## Upgradeability

Neither the pool, nor the backstop, nor the factory exposes an upgrade entrypoint — there is no call in any of the
three that replaces the contract's own WASM. Stock is the same in this respect, so this is not a fork difference; it
is called out here because Precept markets have no backstop to fall back on if a latent bug is found later. The only
way to change a live market's behaviour is for its admin to deploy a new pool and move users to it.

## What is unchanged

The following are the same code as stock, and are documented upstream rather than repeated here:

* the b-dollar/d-dollar accounting, the reserve exchange rates, and the interest-accrual step,
* the utilization-curve interest model,
* user-liquidation auction creation, the Dutch-auction price schedule, the health band it enforces, and partial fills,
* supply, withdraw, borrow, repay, and the health validation that guards them, including the supply-cap check,
* the pool's numeric status model and its automatic backstop-driven transitions,
* the backstop's deposit, withdrawal, donation and draw mechanics,
* the pool factory's deployment flow and its registration of the backstop and reward distributor.

For these, read the [Blend documentation](https://docs.blend.capital).
