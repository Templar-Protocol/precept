# Changes from Blend v2

Precept is a fork of [Blend v2](https://github.com/blend-capital/blend-contracts-v2). This page lists the ways the two
contracts behave differently, with a permalink into the code on both sides of each row. It covers the differences the
fork was built to introduce and the ones a security review of the fork found and fixed. It is written from those
changes rather than generated from a diff, so the two commits pinned below, read against each other, stay the
authority for anything this page does not mention.

| Side | Commit |
| --- | --- |
| Precept | `62c8e9bee072d5e58494c0f7a7bf777926e7ea2b` (the commit these pages are pinned to) |
| Stock Blend v2 | `6cf6dd1ca712ea3cf17a767fe49b7da01f80b909` |

The code released as v0.1.1 matches that commit: the diff between the tag and it touches only proof harnesses, fuzz
targets, build files and test comments — no contract source. v0.1.0 is the earlier, Guardian-remediated code; the
release notes for each version describe what changed between them, including the event-topic changes noted below.

The pattern of the fork is consistent: the code paths a loss would normally travel through are kept, because other
paths call them, and the entrypoints that would let anyone walk into them are switched off. The pool still references
a backstop, still keeps an auction book, and still reads a price feed. The backstop and the bad-debt and interest
auction paths no longer do anything that moves value; user-liquidation auctions stay live, and the price feed only
values positions.

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
| `distribute` | Pulls the emitter's BLND and distributes it to the reward-zone pools and their backstop depositors ([backstop/src/contract.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/backstop/src/contract.rs#L264-L270)) | [backstop/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/backstop/src/contract.rs#L262-L264) |
| `gulp_emissions` | Distributes the emitter's BLND to one reward-zone pool and its backstop, on that pool's authorisation ([backstop/src/contract.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/backstop/src/contract.rs#L272-L279)) | [backstop/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/backstop/src/contract.rs#L266-L268) |
| `add_reward` | Adds a pool to the reward zone, replacing one if the zone is full ([backstop/src/contract.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/backstop/src/contract.rs#L281-L286)) | [backstop/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/backstop/src/contract.rs#L270-L272) |
| `remove_reward` | Removes a pool from the reward zone ([backstop/src/contract.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/backstop/src/contract.rs#L288-L293)) | [backstop/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/backstop/src/contract.rs#L274-L276) |
| `claim` | Claims a user's backstop deposit emissions across a list of pools, minting LP tokens with the BLND claimed ([backstop/src/contract.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/backstop/src/contract.rs#L295-L303)) | [backstop/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/backstop/src/contract.rs#L278-L280) |
| `drop` | Drops the initial BLND to a list of addresses through the emitter ([backstop/src/contract.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/backstop/src/contract.rs#L305-L311)) | [backstop/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/backstop/src/contract.rs#L282-L284) |

The backstop's own deposits, withdrawals, donations and draws are unchanged: the backstop is still a functioning
token-pool contract, it just has no role in settling a loss.

## Auctions: user liquidation only

| Behaviour | Stock | Precept |
| --- | --- | --- |
| Create a bad-debt (1) or interest (2) auction | Creates it ([pool/src/auctions/auction.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/auctions/auction.rs#L75-L94)) | Traps with `PoolError::BadRequest` (1200) ([pool/src/auctions/auction.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/auctions/auction.rs#L109-L111)) |
| Fill a bad-debt or interest auction | Runs the corresponding fill ([pool/src/auctions/auction.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/auctions/auction.rs#L140-L173)) | Traps with `PoolError::BadRequest` (1200) ([pool/src/auctions/auction.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/auctions/auction.rs#L184-L186)) |
| Fill request types 1 / 2 through `submit` | Reaches the corresponding fill ([pool/src/pool/actions.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/actions.rs#L227-L266)) | Reaches the same dispatch and traps inside `fill` ([pool/src/pool/actions.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L273-L312)) |
| Deleting a stale auction, deleting your own liquidation | Deleting an auction type with no live auction traps with `BadRequest` | Same: these two paths are not switched off ([pool/src/auctions/auction.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/auctions/auction.rs#L126-L155)) |

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
| `bstop_rate` at initialisation | Any value below 1.0 is accepted ([pool/src/pool/config.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/config.rs#L198-L202)) | Must be 0 ([pool/src/pool/config.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/config.rs#L27-L29); factory check: [pool-factory/src/pool_factory.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool-factory/src/pool_factory.rs#L74-L76)) |
| `gulp` | Any reserve funds above the accounting expectation are accrued to the reserve's backstop credit and returned as the delta ([pool/src/contract.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/contract.rs#L513-L519), [pool/src/pool/gulp.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/gulp.rs#L18-L37)) | Returns 0 and accrues nothing to the backstop; it only burns pool-owned positions once a reserve carries no debt ([pool/src/pool/gulp.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/gulp.rs#L11-L30)) |

Interest accrual, the utilization curve and the reserve exchange rates are otherwise the same code, so the interest a
borrower pays reaches the reserve's lenders through the b-dollar rate rather than being split with the backstop.

## Position changes

| Behaviour | Stock | Precept |
| --- | --- | --- |
| Withdrawing supply while still borrowing from that reserve | No health check runs on the plain withdrawal path, whatever else the user owes in the reserve ([pool/src/pool/actions.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/actions.rs#L312-L332)) | The health check runs whenever the user holds a liability in that reserve, so a withdrawal that would leave the position short of the health factor bound reverts ([pool/src/pool/actions.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L406-L408)) |
| Supplying collateral in a reserve a borrower does not yet use | No price is read for the reserve being added, so a position can take on a collateral dependency whose feed is unusable, and a later liquidation of that position is blocked while it stays unusable ([pool/src/pool/actions.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/actions.rs#L339-L356)) | The reserve's price is read first when the user carries debt and holds no collateral there yet, so opening that new dependency on an unusable price reverts with `InvalidPrice`; topping up a position the user already holds stays available through an outage ([pool/src/pool/actions.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L442-L447)) |
| Closing a position with an oversized withdrawal | The request is converted to b-tokens before it is capped at the position, so once total defaults have pushed the reserve's b-dollar rate below 1.0 an `i128::MAX` withdrawal overflows in the conversion and reverts until interest lifts the rate again ([pool/src/pool/actions.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/actions.rs#L321-L326), [pool/src/pool/actions.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/actions.rs#L372-L377)) | The request is compared against the value of the position before any conversion, so an oversized request pays out and burns the whole position instead of reverting ([pool/src/pool/actions.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L362-L382), used by both paths at [pool/src/pool/actions.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L409) and [pool/src/pool/actions.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L476)) |

## Pool and reserve configuration

| Behaviour | Stock | Precept |
| --- | --- | --- |
| Reserve configuration after activation | A queued reserve change lands with whatever configuration was queued, for a new or an existing reserve ([pool/src/pool/config.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/config.rs#L98-L111)) | Only `enabled: true → false` is accepted for an existing reserve; every other field must be identical ([pool/src/pool/config.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/config.rs#L115-L134), [pool/src/pool/config.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/config.rs#L211-L227)) |
| Adding a reserve after activation | Possible through the normal queue and timelock | Not possible: queueing a change to a market that is already live reverts unless it is a disable, and a change that would introduce an unknown asset is rejected when it lands ([pool/src/pool/config.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/config.rs#L85-L88), [pool/src/pool/config.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/config.rs#L123-L128)) |
| Supply cap | Enforced when minting b-dollars on the supply and supply-collateral paths, and editable until the pool is activated ([pool/src/pool/actions.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/actions.rs#L300-L302), [pool/src/pool/actions.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/actions.rs#L351-L353)) | The same checks are unchanged; the cap is never editable afterwards, because it is one of the fields the disable-only rule freezes ([pool/src/pool/actions.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L349-L351), [pool/src/pool/actions.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L452-L454)) |
| Per-reserve minimum position | Not present | A reserve carries a `min_position` value, applied per user: a user's collateral position in that reserve must be zero or at least `min_position`, so supplying or withdrawing collateral that would leave a user's non-zero position below it reverts with `MinCollateralNotMet` (1224) ([pool/src/storage.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/storage.rs#L71), [pool/src/pool/actions.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L417-L423)) |
| Admin handover | `propose_admin` stores the proposal in a temporary entry with a 10-day storage TTL, which any account can extend, so the proposal can outlive the ten days ([pool/src/storage.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/storage.rs#L239-L248)) | Same two-step flow, but the expiry is written next to the address as a ledger bound and enforced when the proposal is read, so it lapses ten days after it was made however long the entry is kept alive ([pool/src/storage.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/storage.rs#L293-L320)) |

## Status rules

| Behaviour | Stock | Precept |
| --- | --- | --- |
| Setting status 4 (admin freeze) | Sets it; another `set_status` call can leave it ([pool/src/pool/status.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/status.rs#L71-L117)) | Any `set_status` to anything other than 4 reverts with `StatusNotAllowed`, including by the admin ([pool/src/pool/status.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/status.rs#L84-L141)) |
| Leaving `setup` | No oracle format requirement ([pool/src/pool/status.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/status.rs#L71-L117)) | The configured oracle must report 7 decimals, or `set_status` reverts with `InvalidPrice` ([pool/src/pool/status.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/status.rs#L95-L101)) |

The numeric statuses, their meanings and the automatic transitions driven by the backstop's balance are otherwise
unchanged, including the treatment of an admin freeze from the outside: `update_status` already refuses to run while
the pool is on 4 in stock
([pool/src/pool/status.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/status.rs#L31-L35))
as well as here.

## Price checks

`load_price` accepts a price only if all of the following hold; any failure traps with `PoolError::InvalidPrice`
([pool/src/pool/pool.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/pool.rs#L123-L143)):

| Check | Stock | Precept |
| --- | --- | --- |
| The feed does not answer | The absent price is unwrapped and panics rather than returning an error ([pool/src/pool/pool.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/pool.rs#L119-L131)) | Traps with `PoolError::InvalidPrice` |
| `last_updated` is dated ahead of ledger time | Not checked ([pool/src/pool/pool.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/pool.rs#L119-L131)) | Traps with `PoolError::InvalidPrice` |
| `last_updated` is older than 24 hours | Traps with `InvalidPrice` ([pool/src/pool/pool.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/pool.rs#L126-L128)) | Unchanged |
| Price is zero or negative | Traps with `InvalidPrice` | Unchanged |
| The feed's `decimals()` is not 7 | Not checked; the feed's own scale is cached and used as-is ([pool/src/pool/pool.rs](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/pool.rs#L102-L110)) | Traps with `InvalidPrice` ([pool/src/pool/pool.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/pool.rs#L100-L113)) |

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
is called out here because Precept markets have no backstop to fall back on if a latent bug is found later. Fixing a
bug in any of the three means deploying a new instance and moving users to it.

## What is the same code

The parts of the contracts listed below are stock Blend v2 behaviour, documented upstream rather than repeated here.
Where the fork reorganised stock code without changing what it does, that is noted:

* the b-dollar/d-dollar accounting and the reserve exchange rates. The debt-growth, accrual and utilization
  calculations were moved from the reserve onto its stored data, with the same formulae and rounding,
* interest accrual and the utilization-curve interest model. `calc_accrual` was split into helper functions, but it
  performs the same fixed-point operations with the same rounding in the same order
  ([stock](https://github.com/blend-capital/blend-contracts-v2/blob/6cf6dd1ca712ea3cf17a767fe49b7da01f80b909/pool/src/pool/interest.rs#L23-L100),
  [Precept](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/interest.rs#L23-L117)),
* user-liquidation auction creation, the Dutch-auction price schedule, the health band it enforces, and partial fills,
* supply, borrow, repay and collateral withdrawal, including the health validation on borrow and collateral
  withdrawal, apart from the differences in the tables above. The repayment planning was moved into a helper with the
  same arithmetic,
* the pool's numeric status model and its automatic backstop-driven transitions, apart from the status rows above,
* the backstop's deposit, withdrawal, donation and draw mechanics,
* the pool factory's deployment flow and the backstop and BLND token addresses it passes to each pool, apart from the
  zero take-rate check.

For these, read the [Blend documentation](https://docs.blend.capital). The two commits at the top of this page are the
authoritative comparison.
