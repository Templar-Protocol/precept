# Lending economics

How money moves in a Precept pool: who lends, who borrows, what happens when a borrower goes
underwater, and who ends up holding a loss. Written for lenders, borrowers and integrators.
Code references are permalinks into `Templar-Protocol/precept` at
`62c8e9bee072d5e58494c0f7a7bf777926e7ea2b`.

The one-paragraph version: lenders deposit into a reserve and earn all of the interest borrowers
pay. Borrowers post collateral and borrow against it. If a borrower's collateral falls too far,
anyone can liquidate them by paying off their debt in exchange for the collateral at a discount.
If the collateral runs out before the debt is paid, the gap is taken from the lenders of that
reserve, pro rata, in the same transaction. There is no insurance fund; the "backstop" is
activation plumbing with no value. Prices come from external oracle networks through Templar's
proxy; liquidators are external firms. When prices go stale, every action that has to load a price stops, which is where
liquidations and loss recognition stall; actions that read no price keep working — a partial fill that leaves the filler
with no debt, a write-off where no collateral is left to value, and an ordinary withdrawal by a lender who owes nothing
in that reserve. Section 6 gives the table of which actions load a price.

## 1. Reserves, lenders, borrowers

A pool has one **reserve** per asset (for example USDC, XLM, WETH). Each reserve has:

- a `supply_cap`: the largest a reserve's b-token supply may be, in token units. It is chosen by whoever creates
  the pool and is enforced on every new deposit (interest accrual can carry the total past it, since no deposit is
  involved); once the market is live it cannot be changed, because the rules that govern a live pool let a reserve be
  switched off and nothing else
  ([pool/src/pool/actions.rs:349-351](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L349-L351),
  [pool/src/pool/actions.rs:452-454](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L452-L454),
  [pool/src/pool/config.rs:115-134](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/config.rs#L115-L134));
- a `c_factor` (collateral factor): the share of an asset's value that counts when it is posted as
  collateral — at 0.75, a dollar of XLM counts as 75 cents of borrowing power;
- an `l_factor` (liability factor): the inverse haircut applied to borrowed value;
- an interest curve: the borrow rate rises with utilization (how much of the reserve is lent out).

**Lenders** deposit into a reserve and receive b-tokens. The b-token price (`b_rate`) rises as
interest accrues; a lender's claim is `b_tokens * b_rate`. **Borrowers** post collateral (also
b-tokens, flagged as collateral) and take on d-tokens whose price (`d_rate`) rises with interest.

The backstop take rate is zero on this fork ([pool/src/pool/config.rs:27-29](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/config.rs#L27-L29)), and there is no
protocol fee. Every unit of interest borrowers pay goes to the lenders of that reserve.

## 2. Health factor

A position is healthy while the collateral value after `c_factor` haircuts exceeds the debt
value after `l_factor` haircuts ([pool/src/pool/health_factor.rs:27-84](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/health_factor.rs#L27-L84)). The ratio is the health
factor. Below 1.0 the position is liquidatable.

Both sides are priced with the pool oracle (section 6). A position is only re-checked when its
owner acts (borrow, withdraw collateral, withdraw a plain deposit while holding debt in that reserve
([pool/src/pool/actions.rs:406-408](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L406-L408)),
or delete their own liquidation auction
([pool/src/pool/actions.rs:313-316](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L313-L316)))
or when someone liquidates it. Nothing happens
automatically when a price moves; a keeper has to act.

## 3. Liquidation auctions

Only one auction type exists on this fork: the **user liquidation auction**
([pool/src/auctions/auction.rs:75-97](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/auctions/auction.rs#L75-L97)). The bad-debt and interest auctions that stock Blend uses
to pay its backstop are rejected, because the backstop here has nothing to pay.

### Creating an auction

Anyone can open an auction on an underwater borrower (`new_auction`,
[pool/src/auctions/user_liquidation_auction.rs:12-77](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/auctions/user_liquidation_auction.rs#L12-L77)). The creator names:

- the **bid**: which of the borrower's debts the liquidator will repay;
- the **lot**: which of the borrower's collateral the liquidator will receive;
- a **percent**: how much of the position to liquidate.

The pool checks that the resulting position would land in a reasonable health band. A partial
liquidation must leave the borrower between 1.03 and 1.15 health; above 95% of all positions it is
treated as a full liquidation ([pool/src/auctions/user_liquidation_auction.rs:92-201](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/auctions/user_liquidation_auction.rs#L92-L201)).
This stops liquidators from taking far more
collateral than the debt justifies.

### Dutch auction pricing

An auction runs on a 400-block clock (about 35 minutes at 5-second ledgers)
([pool/src/auctions/auction.rs:291-312](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/auctions/auction.rs#L291-L312)):

| Blocks since creation | Collateral the filler receives | Debt the filler repays |
| --- | --- | --- |
| 0 → 200 | 0% → 100% of the lot, rising 0.5% per block | 100% of the bid |
| 200 → 400 | 100% of the lot | 100% → 0% of the bid, falling 0.5% per block |
| 400 → deletion | 100% of the lot | **0%. The collateral is free to the first caller** ([pool/src/auctions/auction.rs:304-306](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/auctions/auction.rs#L304-L306)) |

Early on the filler pays full debt for a fraction of the collateral; later they pay a fraction of
the debt for all the collateral. Liquidators fill when the trade becomes profitable for them, which
is normally somewhere in the middle. This is the discount lenders implicitly pay for getting the
debt cleared. Past block 200 the filler repays less than the full debt; whatever they do not repay
stays with the borrower, and once the borrower has no collateral left it is written off against
the lenders (section 5). The later a fill lands, the larger that write-off.

### Filling

A filler submits `FillUserLiquidationAuction` with a `percent_filled` between 1 and 100
([pool/src/auctions/auction.rs:175-215](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/auctions/auction.rs#L175-L215)). They take on that share of the bid (the debt) and receive
that share of the lot (the collateral). The filler's own position must be healthy at the end of
their transaction, so in practice a liquidator either has the cash to repay the debt in the same
`submit`, or holds enough of their own collateral to carry it briefly.

### Stale auctions and the free-collateral window

From block 400 until the auction is deleted, anyone can take the entire remaining lot for
nothing. The filler repays a bid that has scaled to zero, so the borrower's debt is not repaid by that fill; whether
it lands on the lenders (section 5) depends on what the borrower has left. The bad-debt handler runs on a full fill,
and it returns without defaulting anything while the borrower's remaining collateral still has value
([pool/src/auctions/user_liquidation_auction.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/auctions/user_liquidation_auction.rs#L218-L220),
[pool/src/pool/bad_debt.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/bad_debt.rs#L60-L74)).
The auction can be removed by the borrower's own `DeleteLiquidationAuction` — no age check, but it requires the pool
to be at status 0 or 1 and the borrower's remaining positions to pass the health check afterwards
([pool/src/pool/actions.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L313-L322),
[pool/src/auctions/auction.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/auctions/auction.rs#L150-L155),
[pool/src/pool/pool.rs:76-80](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/pool.rs#L76-L80),
[pool/src/pool/pool.rs:146-154](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/pool.rs#L146-L154))
— or by anyone via `delete_stale_auction` once 500 blocks have passed
([pool/src/auctions/auction.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/auctions/auction.rs#L126-L138)).
A borrower still under the liquidation threshold therefore has no way to cancel the free-fill window in blocks
400–500: self-deletion fails their health check and no third party can delete yet.

No participant is obliged to prevent an auction reaching block 400: liquidators are external
(section 6) and borrower deletion is voluntary. Nothing on-chain enforces that an auction is filled or
deleted before block 400, and no third party can be ordered to prevent it. "Let it expire and
recreate it" is not a remedy: recreation needs
a fresh oracle price, which a stale-oracle scenario will not supply, and the lot has already been
given away.

## 4. Partial fills and liquidator capital

Partial fills mean a liquidator does not need capital equal to the whole position. The loop is:

1. fill, say, 10% of the auction: repay 10% of the debt, receive 10% of the collateral;
2. sell the collateral on the Stellar DEX for the debt asset;
3. repeat until the auction is gone.

A liquidator with capital for a tenth of a position can clear it in ten rounds, provided each round fits inside the
auction clock and the collateral can be sold on the DEX without moving the price much. Those two
constraints, not the total size of the position, decide whether a liquidation succeeds:

- **Time.** Fills that land after block 400 repay nothing (section 3, "Stale auctions"). Partial
  fills shrink the existing auction; they do not restart its clock. Any lot still unfilled at 400
  is given away and, if what the borrower keeps values to zero, its debt lands on the lenders at the next
  settlement, and a recreated auction needs a fresh oracle
  price (section 6).
- **DEX depth.** If the collateral is thin on the DEX, selling it pushes the price down and the
  next fill recovers less. This is where a liquidation "fails to sell for enough".

The contracts put no ceiling on a single position. The `supply_cap` limits how much a reserve can hold, and once a
market is live that number cannot be raised, but the practical bound on a position is different: how much of the
collateral asset can be sold on Stellar inside roughly 30 minutes without a large price impact, and whether the
liquidators active for that asset have per-round capital that, times the rounds that fit in the clock, covers the
largest position that bound allows. The cap each market ships with is published per reserve on
[Official markets](official-markets.md).

## 5. When the collateral runs out: lenders take the loss

If a full fill leaves the borrower with debt but no collateral worth anything, the fork runs the
bad-debt handler in the same transaction
([pool/src/auctions/user_liquidation_auction.rs:218-219](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/auctions/user_liquidation_auction.rs#L218-L219),
[pool/src/pool/bad_debt.rs:54-118](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/bad_debt.rs#L54-L118)).
Anyone can also trigger it later with `bad_debt(user)` once no auction is live.

In order:

1. **Set-off.** If the borrower also has ordinary (non-collateral) deposits in the same reserve
   they owe, those are burned against the debt first; a supply that covers the debt in full settles it here, with no
   default. Event: `debt_setoff`, with topics
   `[name, user, asset]` ([pool/src/events.rs:185-199](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/events.rs#L185-L199)).
2. **Default to lenders.** Whatever debt remains is written off against the lenders of that
   reserve by lowering `b_rate` ([pool/src/pool/user.rs:110-160](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/user.rs#L110-L160),
   [pool/src/pool/user.rs:309-316](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/user.rs#L309-L316)). Every lender in the
   reserve loses the same percentage, immediately. Event: `defaulted_debt`, with topics
   `[name, user, asset]` and the defaulted d-token amount as data
   ([pool/src/events.rs:179-183](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/events.rs#L179-L183)).
3. **Orphan the dust.** If a debt was actually defaulted, any collateral the borrower still holds is moved to a
   pool-owned position nobody can withdraw, so a later price move cannot revive it for the borrower. A same-reserve
   supply that repays the debt in full defaults nothing, and that collateral stays with the borrower
   ([pool/src/pool/bad_debt.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/bad_debt.rs#L96-L116)).
   Event: `collateral_orphaned`, with topics `[name, user, asset]`
   ([pool/src/events.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/events.rs#L201-L204)).

What this means for a lender:

- Your claim is `b_tokens * b_rate`. A default lowers `b_rate` for your reserve only; other
  reserves are untouched.
- The loss can never exceed what the reserve's lenders hold: the pool can only lend out cash that
  lenders deposited, so a default of the entire reserve's value is the worst case. The write-off lowers
  `b_rate` and the contract floors it at zero rather than letting it go negative
  ([pool/src/pool/user.rs:309-323](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/user.rs#L309-L323));
  a reserve emptied this way has nothing left to lend.
- There is no junior tranche, no insurance module, no protocol treasury cure. The backstop contract
  exists because a Blend pool needs one to activate; the assets it can hold are fixed by its
  constructor, and nothing obliges it to hold any of them for a pool's benefit. Anyone describing it
  as insurance is wrong.
- Between the moment a borrower goes underwater and the moment the loss is written, lenders with
  no debt in that reserve can still withdraw at full value. The pool does not hold back cash for a
  pending loss. The slowest lenders in a bad reserve take more of the loss. This is disclosed, not
  prevented.

This is the Morpho Blue model: lenders earn the whole interest and are the only loss-absorbing
layer. It is a different model from Aave, where a staked-token safety module takes the first loss.

Why the fork does it this way rather than stock Blend's "send bad debt to the backstop": with a
worthless backstop, stock behaviour would park the bad debt in a backstop auction that no one would
ever fill. The lenders would still lose the money, but slowly and invisibly, with the last lender
to withdraw losing the most. Writing the loss immediately is the honest version of the same
outcome.

## 6. Oracle and keepers

### What the pool checks

Every price is read from the pool's oracle (Templar's proxy adapter) at the moment a position is
valued ([pool/src/pool/pool.rs:123-180](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/pool.rs#L123-L180)). The pool rejects a price that is:

- missing — a feed that does not answer is an error, not a default;
- dated in the future;
- more than 24 hours old (exactly 24 hours is accepted);
- zero or negative;
- not 7 decimals.

The proxy applies its own limit on top: each asset has a maximum accepted age, above which the
proxy has no price to give. Each official market publishes that `max_age_secs` on
[Official markets](official-markets.md).

### What stops when a price is stale

A stale price only affects positions that hold that asset, but for those positions:

| Action | Blocked by a stale price? |
| --- | --- |
| Deposit, repay | No, unless the depositor has any debt and is supplying collateral into a reserve where they hold none ([pool/src/pool/actions.rs:442-447](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L442-L447)) |
| Withdraw ordinary deposit | No, unless the lender also has debt in that reserve |
| Withdraw collateral, borrow | Yes, if the account has any debt |
| Create a liquidation auction | **Yes** |
| Partial fill of an existing auction | No, as long as the filler ends with no debt ([pool/src/pool/submit.rs:214-218](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/submit.rs#L214-L218)) |
| The **full fill** that completes the lot (it runs the write-off itself) | **Yes** while the borrower still holds collateral to value; a partial fill does not |
| `bad_debt(user)` | Yes, unless the borrower has no collateral left to value ([pool/src/pool/bad_debt.rs:63-74](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/bad_debt.rs#L63-L74)) |
| Delete a stale auction | No |

So a stale oracle freezes liquidations and loss recognition while leaving ordinary withdrawals by
lenders with no debt in that reserve open — the table above gives the exceptions. That widens the window in section 5.
Oracle liveness is therefore a lending-safety
requirement, not a convenience.

### Oracle sources and refresh

Prices come from external oracle networks published into Templar's proxy, which aggregates the
independent sources configured for each asset — at least three are required for an asset to be
configured at all
([contract/proxy-oracle/soroban/common/src/lib.rs:159-166](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/common/src/lib.rs#L159-L166))
— and exposes the low-middle median of the sources that are within their accepted age
([contract/proxy-oracle/soroban/contract/src/conversion.rs:132-143](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/contract/src/conversion.rs#L132-L143)).
A pool reads that through the adapter in this repository
([pool-oracle-adapter/src/lib.rs:51-134](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool-oracle-adapter/src/lib.rs#L51-L134)),
which is fixed at construction: no owner, no upgrade, and a decimal count that cannot be changed
afterwards. It refuses a parent price dated ahead of ledger time
([pool-oracle-adapter/src/lib.rs:194-197](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool-oracle-adapter/src/lib.rs#L194-L197)).
No contract in this repository produces prices. Two things still have to be done by somebody, and both are
permissionless: calling the proxy's `refresh` often enough that its newest accepted price stays inside the asset's
`max_age_secs`, and extending the oracle, pool and backstop instances so their ledger entries are not evicted. The
adapter's own instance is extended when it is read and when anyone calls its public `extend_ttl`
([pool-oracle-adapter/src/lib.rs:112-114](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool-oracle-adapter/src/lib.rs#L112-L114)),
so in a quiet market with no borrows, liquidations or reads nothing extends it unless somebody calls
`extend_ttl`, and it can be evicted while the external feeds are perfectly healthy. Somebody has to do
both, every market, continuously — an unattended oracle is an oracle that eventually stops answering.

### Liquidators

Liquidators are external, profit-seeking operators: established liquidation firms, and anyone
else who wants to. They watch health factors, open auctions on underwater borrowers, and fill them
when the lot is worth more than the bid. Templar does not run them and cannot direct their timing.
They are nonetheless the only thing that turns "lenders may lose money" into "lenders rarely lose
money": the faster an underwater position is cleared, the smaller the chance the collateral falls
below the debt before it is sold.

Their incentive and the lenders' are aligned for most of the curve in section 3 and diverge at the
end of it. For the first 200 blocks the filler repays the full debt and receives a growing
fraction of the collateral, so a fill only becomes profitable once that fraction, sold on the DEX,
exceeds the debt: late in the first 200 blocks for a position just past the liquidation threshold,
after 200 for one that has fallen further. From 200 onwards the lot is whole and the debt repaid
shrinks 0.5% per block, so every block a filler waits moves lender value to the filler. A filler
who can wait to block 399 takes all the collateral for 0.5% of the debt and hands the other 99.5%
to the lenders. What bounds the waiting is the next competitor: the first fill wins, so nobody can
afford to wait long. What that leaves is a market dependency, not a property of the contract: whether a given
market has several independent operators watching its auctions is an observable fact (fill events name their
callers), and a market with only one is one outage away from the block-400 outcome above.

The structural controls are the parameters, not the operators. A wide gap between a borrower's actual borrow ratio
and the `c_factor` ceiling is what survives a ~35-minute auction plus fill latency, and a `supply_cap` sized to the
depth of that collateral market keeps positions small enough to clear in a few rounds.

## 7. Parameters that decide how often lenders lose

In rough order of importance:

1. `c_factor` per collateral asset sets the borrowing ceiling; the buffer against liquidation is
   the distance between a borrower's actual borrow ratio and that ceiling, not the gap between
   `c_factor` and 100%. A borrower who borrows up to the ceiling has no buffer at all, which is
   why partial liquidation targets a health factor of 1.03–1.15 rather than 1.00. What `c_factor`
   does bound is the lenders' side: once a position is liquidatable, the collateral can fall a
   further `1 - c_factor` (25% at 75%, 10% at 90%) before the debt exceeds it.
2. `supply_cap` per reserve, sized to the depth of that collateral market (section 4).
3. Proxy `max_age_secs` and the cadence of the refresh operators (section 6).
4. How many independent operators are actually filling that asset's auctions, and with what capital and
   response time; a market dependency, not a parameter the contracts set (sections 4 and 6).
5. The interest curve, which decides what lenders are paid for carrying this risk.

Each mainnet reserve's parameters — factors, caps and interest curve — are published on
[Official markets](official-markets.md). Configuration values used in test fixtures are not
production parameters and are not published here.
