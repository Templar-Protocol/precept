# How Precept works

Precept is a per-market lending pool on Soroban. Each market is its own pool contract with its own reserve list and
its own admin. A pool references a backstop contract and a pool factory, but a loss in one pool never reaches the
lenders of another: every loss is settled inside the reserve it came from.

## What Precept changes about a Blend pool

A stock Blend pool has three ways to absorb a shortfall: a backstop that can fund a liquidation or take on bad debt,
auctions that sell pool assets to raise the shortfall, and a backstop share of interest that funds it. Precept
switches the first two off and sets the third to zero.

* **No backstop funding, no backstop insurance.** The backstop's reward distribution and its emissions claim are
  trapped, and no reachable pool path asks it for money: the code that would draw on it or pay it sits behind auction
  types that can no longer be created
  ([pool/src/auctions/auction.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/auctions/auction.rs#L184-L186)).
  Which assets a backstop can ever hold is fixed once, by the constructor of the backstop and its token
  ([backstop/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/backstop/src/contract.rs#L173-L195)).
  The backstop is plumbing, not protection.
* **Only user-liquidation auctions.** Interest and bad-debt auctions cannot be created or filled
  ([pool/src/auctions/auction.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/auctions/auction.rs#L109-L111)).
* **No split with the backstop.** The backstop's share of interest (`bstop_rate`) must be 0 when a pool is deployed
  ([pool/src/pool/config.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/config.rs#L27-L29)),
  and `gulp` no longer accrues reserve funds to it: it returns 0 and only burns pool-owned positions once a reserve
  carries no debt
  ([pool/src/pool/gulp.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/gulp.rs#L11-L30)).
  Interest accrues from the elapsed time since a reserve last accrued, and reaches lenders through the b-dollar rate.
* **Losses go to the reserve's lenders.** When a defaulted position cannot be repaid out of the defaulted user's own
  assets, the remaining debt is written off against the b-dollar exchange rate of that same reserve
  ([pool/src/pool/bad_debt.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/bad_debt.rs#L54-L118)).
  Lenders of other reserves are untouched.

For everything that is unchanged — the b-dollar/d-dollar accounting, the interest-rate curve, how a liquidation
auction is priced and scaled — see the upstream [Blend documentation](https://docs.blend.capital).

## An isolated position

A user's exposure is a pair of vectors over the pool's reserve list: b-token balances (collateral) and d-token
balances (liabilities). The pool prices every non-zero entry through its oracle and computes

```text
health factor = sum over collateral reserves of effective collateral, in x-rats
                -------------------------------------------------------------
                sum over liability reserves of effective liabilities, in x-rats
```

where `x-rats` is 7 decimals ([pool/src/pool/health_factor.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/health_factor.rs#L27-L110)).
"Effective" means the b-dollar and d-dollar quantities are converted with the reserve's exchange rates and scaled by
that reserve's collateral factor.

Two floors apply to anyone with open debt. A borrow, a collateral withdrawal, an auction fill, the removal of one's
own liquidation auction, or a plain withdrawal from a reserve the account is already borrowing in re-checks health,
and the action fails with `InvalidHf` if the account would end up below 1.0, and with `MinCollateralNotMet` if its
effective collateral would fall below the pool's `min_collateral`
([pool/src/pool/actions.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L253-L322),
[pool/src/pool/actions.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L406-L408),
[pool/src/pool/actions.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L467-L515),
[pool/src/pool/submit.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/submit.rs#L212-L238));
`min_collateral` is a plain non-negative number chosen when the pool is created
([pool-factory/src/pool_factory.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool-factory/src/pool_factory.rs#L84-L87))
and cannot be changed afterwards. A supply-collateral deposit is not health-checked: it reads the reserve's price
only when a borrower takes collateral in a reserve it did not already hold, and refuses the deposit if that price
is unusable ([pool/src/pool/actions.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L433-L457)).
A position is liquidatable below 1.0, and a *partial* liquidation must leave the
borrower between 1.03 and 1.15 health — outside that band the auction is rejected as too small or too large
([pool/src/auctions/user_liquidation_auction.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/auctions/user_liquidation_auction.rs#L92-L201));
a request that covers all of the borrower's positions and more than 95% of them is treated as a full liquidation
instead.

There is no separate liquidator address in Precept. Anyone who can fill the auction is the liquidator, and filling it
is an ordinary pool operation — the bidder's request goes through the same `submit` entrypoint as any other
([pool/src/pool/submit.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/submit.rs#L36-L77)).
Filling is not gated on the pool's status, so a frozen market can still be cleared; what status does gate is opening
new debt — borrows and auction cancellations stop at any status above 0/1, and new supplies stop once the pool is
frozen
([pool/src/pool/pool.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/pool.rs#L151-L154)).

## What a liquidation fills against

A liquidation auction is a quote, not a swap. The seller side is a quantity of the defaulted user's b-tokens (a
"lot"); the buyer side is an amount of d-tokens in the reserve being repaid. Both are pool-generated quotes, denominated
in pool tokens rather than in x-rats.

* Bids are in d-tokens: a bidder repays debt and receives b-token collateral.
* The lot is in b-tokens, so what a bidder receives is the b-token balance left in the reserve — not a promise of a
  fixed amount of the underlying asset. A bidder therefore bears the reserve's existing shortfall, plus any other bad
  debt written off in that reserve while it was being cleared.
* The terms of the quote follow a Dutch-auction schedule on the auction's block counter — the lot rises against the
  bid, and once the lot is whole the bid falls — while the underlying exchange rates are read from live pool state at
  creation and at fill
  ([pool/src/auctions/auction.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/auctions/auction.rs#L291-L312)).
* Partial fills are allowed and a partially filled auction stays open with the remainder, so a bidder pays for what
  it took.

A bidder with an existing position is not forced to liquidate itself: if a fill would push the bidder's own health
factor to the liquidation threshold or below, the fill is rejected. A borrower can have an auction against them
removed at any age — their own `DeleteLiquidationAuction` only requires that their remaining positions then pass the
health check, and that the pool is at status 0 or 1
([pool/src/pool/actions.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L313-L322),
[pool/src/auctions/auction.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/auctions/auction.rs#L150-L155),
[pool/src/pool/pool.rs:76-80](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/pool.rs#L76-L80),
[pool/src/pool/pool.rs:146-154](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/pool.rs#L146-L154))
— while anyone can remove an auction once it is 500 blocks old
([pool/src/auctions/auction.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/auctions/auction.rs#L126-L138)).

## When there is nothing left to bid

A defaulted position whose collateral is worth nothing is settled in one `bad_debt` call, in a fixed order
([pool/src/pool/bad_debt.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/bad_debt.rs#L11-L118)):

1. **Setoff.** Whatever the user supplied in the liability reserve itself cancels the debt directly, and emits
   `debt_setoff` with topics `[name, user, asset]` and the amounts in the data
   ([pool/src/events.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/events.rs#L188-L198)).
   If that supply repays the debt in full, the default and orphan steps do not run and the collateral stays with the
   user ([pool/src/pool/bad_debt.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/bad_debt.rs#L96-L116)).
2. **Default to the reserve's lenders.** The remaining debt lowers that reserve's b-dollar exchange rate, so every
   b-token holder in it absorbs it pro rata. This emits `defaulted_debt` with topics `[name, user, asset]` and the
   defaulted d-token amount in the data
   ([pool/src/events.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/events.rs#L179-L182)).
3. **Orphan the collateral.** Any collateral the defaulted user still holds is moved out of their account into a
   pool-owned position. This emits `collateral_orphaned` with topics `[name, user, asset]` and the orphaned b-token
   amount in the data
   ([pool/src/events.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/events.rs#L201-L204)).

Nothing is sold on the defaulted user's behalf. The orphaned b-tokens remain part of that reserve's b-token supply and
belong to the pool; they are burned, with an `orphan_settled` event, once the reserve carries no debt at all
([pool/src/pool/gulp.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/gulp.rs#L11-L30),
[pool/src/events.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/events.rs#L207-L210)).

The condition for this path is that the user's remaining collateral values to zero, or that the user holds no
collateral at all; when there is no collateral to value, prices are not loaded
([pool/src/pool/bad_debt.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/bad_debt.rs#L63-L74)).
"Nothing left to bid" in practice means nobody bid, the reserve has nothing left to sell, or the backstop holds
nothing — and that last one is a normal condition in Precept, not a backstop failure, because the backstop is never
asked.

`bad_debt` only runs after the position has been cleared through auctions: it reverts while a user-liquidation
auction for that user is still open
([pool/src/pool/bad_debt.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/bad_debt.rs#L20-L22)).
A fill that takes the last of a user's auction runs the bad-debt handler in the same transaction
([pool/src/auctions/user_liquidation_auction.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/auctions/user_liquidation_auction.rs#L218-L220)),
so such a position is settled in that one call; a position left with debt and nothing to value after a partial fill —
or after nobody filled at all — is settled by a later `bad_debt(user)` call. Either way the events show exactly what
was set off, what was socialised, and what was orphaned.

## Who runs the pool, and which levers stay

A pool is deployed from the factory by the account that will administer it, which supplies the pool name, a creation
salt, the oracle address, the backstop's share of interest (which must be zero), the maximum number of positions and
the minimum collateral for a borrow. The backstop and the BLND token addresses are not chosen per pool: they come from
the factory's own configuration, fixed when the factory itself was deployed
([pool-factory/src/pool_factory.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool-factory/src/pool_factory.rs#L59-L107),
[pool-factory/src/storage.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool-factory/src/storage.rs#L19-L25));
reserves are configured afterwards, by the admin, before the market is activated. The admin's levers after activation
are deliberately narrow, and they are listed in full on [Risks](risks.md). The points that matter to a lender:

* Reserve settings are **disable-only after activation**. Once a market is live, an existing reserve's collateral
  factor, liability factor, target and maximum utilization, interest-curve parameters (`r_base`, `r_one`, `r_two`,
  `r_three`, `reactivity`), supply cap, minimum position size, decimal count and index cannot be changed
  ([pool/src/storage.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/storage.rs#L58-L73),
  [pool/src/pool/config.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/config.rs#L211-L227)),
  and a reserve can only be turned off — never back on. No new reserve can be introduced either
  ([pool/src/pool/config.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/config.rs#L85-L88),
  [pool/src/pool/config.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/config.rs#L115-L134)).
* Everything else about a live pool is likewise frozen. Stock Blend's `update_pool` — the entrypoint that would move
  the oracle address, the take rate, the position limit or the collateral minimum — is switched off
  ([pool/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/contract.rs#L406-L408)).
  A market cannot even leave its `setup` status unless the oracle it was configured with reports 7 decimals
  ([pool/src/pool/status.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/status.rs#L95-L101)).
* The admin can pause and resume the market. `set_status` accepts 0 (active), 2 (on-ice: new borrows and auction
  cancellations stop), 3 (permissionless on-ice) and 4 (admin freeze: only withdrawals, repayments and auction fills
  continue). **Setting 4 is irreversible** — any later `set_status` to anything other than 4 reverts
  ([pool/src/pool/status.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/status.rs#L84-L141)).
* The admin's only remaining power is to hand the pool over: `propose_admin` then `accept_admin`, with a proposal that
  lapses after ten days
  ([pool/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/contract.rs#L383-L404),
  [pool/src/storage.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/storage.rs#L290-L320)).
* The pool, the backstop and the factory hold their compiled code forever: none of them exposes an upgrade entrypoint.
  Changing a live contract's behaviour means deploying a new pool and moving users to it.
