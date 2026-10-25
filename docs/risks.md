# Risks

Precept removes the safety net that a stock Blend pool borrows from a backstop. What is left is the pool's own
accounting, the market's bidders, the oracle, and a small set of admin and governance levers. This page lists each of
them and what it can and cannot do to your deposit.

Nothing here is a promise about a live market: [no official Precept market is live yet](official-markets.md).

## What is not protecting a lender

* **The backstop is not insurance.** Its reward distribution and its emissions claim are switched off
  ([backstop/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/backstop/src/contract.rs#L262-L284)),
  and no reachable pool path asks it for money: there is no backstop contribution to a liquidation and no backstop bad
  debt. Which assets a backstop can hold is fixed once, by its constructor
  ([backstop/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/backstop/src/contract.rs#L173-L195)).
* **There is no rescue auction.** Interest and bad-debt auctions are switched off
  ([pool/src/auctions/auction.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/auctions/auction.rs#L109-L111)),
  so a shortfall is never converted into a sale of pool assets at a discount to someone else.
* **There is no protocol fee and no insurance fund to draw on.** The backstop's share of interest is fixed at 0 when
  a pool is deployed
  ([pool/src/pool/config.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/config.rs#L27-L29)),
  and `gulp` cannot move reserve funds to the backstop
  ([pool/src/pool/gulp.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/gulp.rs#L11-L30)).

The honest summary: if a borrower cannot repay, the loss is taken out of the exchange rate of the b-dollars of the
reserve that was lent, and it falls on that reserve's lenders pro rata, in proportion to their claims
([pool/src/pool/bad_debt.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/bad_debt.rs#L86-L102),
[pool/src/pool/user.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/user.rs#L110-L160)).
No other lender group, and no backstop, absorbs it.

## What can cost a lender money

| Path | What happens | Who or what stands in the way |
| --- | --- | --- |
| A borrower defaults and the reserve is short | The reserve's b-dollar exchange rate is lowered; every b-token holder in that reserve loses proportionally | Liquidation bidders, if they bid; nobody else |
| A liquidation is under-collateralised | Your b-dollars of that reserve absorb the shortfall left by the fills that never happened, and the auction lot is quoted in b-tokens | Nothing |
| The oracle stops publishing | Any operation that reads a price traps, including withdrawals for a user with an open position | The oracle's own governance and its source set |
| The pool admin freezes the market | Status 4 stops new borrows, supplies and cancellations; withdrawals, repayments and auction fills continue | Nothing — the decision is one-way |
| The backstop's position moves sharply | Automatic on-ice/freeze statuses depend on the backstop's balance and on how much of it is queued for withdrawal | Nothing forces the backstop to stay |
| An asset is disabled | Deposits, collateral deposits and borrows of that one asset stop | The pool admin, who can disable it |
| Direct token transfers to the pool | They are not credited to lenders or to the backstop and are not recoverable | Nothing — this is the documented policy |

The last row is worth reading twice: the pool's balance reconciliation is accounting-based
([pool/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/contract.rs#L206-L216)),
so sending a token directly to a pool contract does not make its lenders richer.

## The oracle is the single point of failure

Every operation that checks health loads a price: borrow, supply collateral, liquidate, and — because a withdrawal of
collateral by a user with open debt re-checks that user's health — most withdrawals too. A price is accepted only if the
oracle answers, is not older than 24 hours, is not dated in the future, is positive, and reports 7 decimals
([pool/src/pool/pool.rs:123-143](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/pool.rs#L123-L143),
[pool/src/pool/pool.rs:100-113](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/pool.rs#L100-L113));
any failure traps with `InvalidPrice`, and a missing price traps rather than returning a default. Prices are read only
when a reserve is actually used by an operation, so a reserve can be configured with a feed that has never been
exercised
([pool/src/pool/health_factor.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/health_factor.rs#L57-L84)).
When a price is refused, lending and borrowing for that market stop; a lender with no open position can still exit,
because a plain withdrawal does not read prices.

Precept markets price through the pool-oracle adapter in this repository, which reads a Templar proxy oracle
([pool-oracle-adapter/src/lib.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool-oracle-adapter/src/lib.rs#L51-L134)).
The adapter is fixed at construction: it has no admin, no upgrade and no owner, its decimal count cannot be changed
afterwards, and it refuses a parent price dated ahead of ledger time
([pool-oracle-adapter/src/lib.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool-oracle-adapter/src/lib.rs#L194-L197)).
The parent proxy aggregates a set of independent sources configured for each asset — an asset needs at least three
sources to be configured at all
([contract/proxy-oracle/soroban/common/src/lib.rs:159-166](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/common/src/lib.rs#L159-L166))
— and publishes the low-middle median of the sources that are fresh enough
([contract/proxy-oracle/soroban/contract/src/conversion.rs:132-143](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/contract/src/conversion.rs#L132-L143)).
Each asset also has a maximum accepted age, capped at a hard ceiling of seven days
([contract/proxy-oracle/soroban/common/src/lib.rs:155-173](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/common/src/lib.rs#L155-L173));
beyond it the proxy simply has no price to give.

Consequences to price honestly:

* A stale proxy answer (up to its configured maximum age) is still "fresh enough" for the pool, which accepts anything
  under 24 hours. Lending is not protected against a stale-but-valid price by the pool's own checks; it is protected
  only by how the proxy is configured and by how its sources behave.
* The proxy enforces the age limit, and the pool enforces the 24-hour limit. Neither of them judges whether a price
  is *correct* — that is what the source set and its median are for.
* Because the pool traps on a refused price rather than using a stale one, an oracle outage is a liveness failure first
  and a mispricing risk second.
* The proxy has a price only if something has refreshed it. `refresh` is permissionless, and so is keeping the
  contract's ledger entry alive, so an unattended oracle eventually stops answering.

## The levers that exist

### Pool admin

Set when the pool is deployed from the factory, and transferable only by a two-step proposal
([pool/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/contract.rs#L383-L404);
a proposal lapses after ten days
([pool/src/storage.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/storage.rs#L290-L320))). That is the whole list:

| Lever | Effect | Bound |
| --- | --- | --- |
| `set_status` | 0 active, 2 on-ice, 3 permissionless on-ice, **4 admin freeze** | **4 is permanent** — any later `set_status` other than 4 reverts, even the admin's ([pool/src/pool/status.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/status.rs#L84-L141)) |
| Queue / cancel a reserve change | Switch an existing reserve off, behind a timelock | After activation that is the only change an existing reserve can undergo: no parameter of a live reserve can be edited, including its supply cap, a disabled reserve cannot be re-enabled, and no new reserve can be added ([pool/src/pool/config.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/config.rs#L85-L88), [pool/src/pool/config.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/config.rs#L115-L134)) |
| `propose_admin` / `accept_admin` | Hand the pool to another admin | Two signatures required |

Nothing else in a live pool is editable. The oracle address, the backstop's interest share, the position limit and the
collateral minimum are fixed at deployment: stock Blend's `update_pool`, the entrypoint that would move them, is
switched off ([pool/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/contract.rs#L406-L408)).
A pool also cannot leave its `setup` status unless the oracle it was configured with reports 7 decimals
([pool/src/pool/status.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/status.rs#L95-L101)),
and every price read is rejected unless it is exactly 7 decimals
([pool/src/pool/pool.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/pool.rs#L100-L113)).

The admin has no entrypoint that moves a lender's or borrower's funds. `draw` and `donate` sit on the backstop, not on
the reserves: a draw is authorised by the pool and reduces the backstop's position in it — which is exactly the event
that trips the automatic on-ice statuses below — and never touches what lenders can redeem
([backstop/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/backstop/src/contract.rs#L288-L295),
[backstop/src/backstop/fund_management.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/backstop/src/backstop/fund_management.rs#L10-L20)).

### Disabled assets

A reserve has an `enabled` flag, not a separate administrator. Supplying, supplying collateral and borrowing all fail
with `ReserveDisabled` when the flag is off
([pool/src/pool/reserve.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/reserve.rs#L222-L227)).
Withdrawing that asset and filling auctions against it are not gated on it: an existing lender can still leave, and an
underwater position can still be cleared. Only the pool admin changes the flag, and after activation only in the
disable direction — an existing reserve can be turned off, and a reserve that is off cannot be turned back on
([pool/src/pool/config.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/config.rs#L115-L134),
[pool/src/pool/config.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/config.rs#L211-L218)).

### Pool factory

The factory holds no admin rights over anything it has deployed and cannot change a live pool. Its entrypoints are
`deploy` and `is_pool`, and `deploy` authenticates the account it is *about to make* the new pool's admin — there is no
factory-side allowlist
([pool-factory/src/pool_factory.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool-factory/src/pool_factory.rs#L59-L87)).

A pool's name, its token symbols and its user-facing text are all chosen by whoever deployed it, so a Precept-shaped
market is not necessarily *the* Precept market — check a contract address against
[Official markets](official-markets.md) before depositing.

### Backstop

The backstop is not a guarantor, but its position still moves the pool: the pool's automatic status logic moves a
market to on-ice or frozen based on the backstop's balance for that pool and on how much of the backstop's deposits are
queued for withdrawal, and any account can trigger that recomputation
([pool/src/pool/status.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/status.rs#L16-L79),
[pool/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/contract.rs#L171-L189)).
The activation threshold is computed from the value of the backstop's own token holdings
([backstop/src/backstop/pool.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/backstop/src/backstop/pool.rs#L24-L82),
[backstop/src/backstop/pool.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/backstop/src/backstop/pool.rs#L106-L116)),
so an under-funded backstop keeps a pool on-ice rather than active — which limits new borrowing rather than
endangering existing deposits.

Nothing obliges the backstop to stay. Deposits and withdrawals are authorised by whoever holds the backstop's shares —
there is no backstop admin to approve them
([backstop/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/backstop/src/contract.rs#L205-L223))
— and a draw is authorised by the pool itself. A withdrawal is not instant: it must be queued and then sits behind a
17-day lock
([backstop/src/constants.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/backstop/src/constants.rs#L13-L14),
[backstop/src/backstop/withdrawal.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/backstop/src/backstop/withdrawal.rs#L52-L90)),
and it is refused outright while the backstop still carries liabilities to that pool. That delay is the backstop's
commitment mechanism, and it is the only one.

### Oracle governance

The proxy oracle is governed; the adapter in front of it is not — the adapter is a fixed, ownerless translation of one
proxy call into a SEP-40 price. What the oracle's governance contract can still change, and what it cannot:

| Power | How it works | Reference |
| --- | --- | --- |
| Change the sources configured for an asset, or an asset's maximum accepted age | A proposal is created, waits out a time-to-live, then is executed | [contract/proxy-oracle/soroban/governance-contract/src/engine.rs:114-142](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/governance-contract/src/engine.rs#L114-L142) |
| Halt an asset's price | A manual trip on one asset, which also waits for its proposal time-to-live before it is executed | [contract/proxy-oracle/soroban/governance-contract/src/engine.rs:185-212](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/governance-contract/src/engine.rs#L185-L212) |
| Replace the runtime contract, or change which source contracts feed an asset | Only the runtime contract's owner may call these; the owner is the governance contract itself | [contract/proxy-oracle/soroban/contract/src/lib.rs:131-196](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/contract/src/lib.rs#L131-L196) |
| Appoint or revoke governance roles | The governance admin, whose address is fixed at initialisation | [contract/proxy-oracle/soroban/governance-contract/src/lib.rs:99-135](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/governance-contract/src/lib.rs#L99-L135) |

The waiting time is a parameter, not a fixed floor: proposals expire and re-queue against a time-to-live set when the
governance contract is initialised and adjustable through the same proposal path
([contract/proxy-oracle/soroban/governance-common/src/lib.rs:331-335](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/governance-common/src/lib.rs#L331-L335)).
Actions are authorised against a single required role per action type (or the admin), so the split between who may
propose and who may execute is a configuration choice, not a property of the contract
([contract/proxy-oracle/soroban/governance-contract/src/engine.rs:30-42](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/governance-contract/src/engine.rs#L30-L42)).

What no one can change about a live market: the pool's oracle address is fixed at deployment, and the 7-decimal price
format is required by the pool's own checks.

## What nobody can change

The pool, the backstop and the pool factory have no upgrade entrypoint — there is no function in any of the three that
replaces their compiled code, and the only way to change a live market's behaviour is to deploy a new pool and move
users to it. A latent bug found after a market is live stays in that market for its whole life.

## What anyone can do

Precept has no privileged liquidator. These are open to any account:

* Open a user-liquidation auction for an under-collateralised position, and fill auctions
  ([pool/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/contract.rs#L565-L591)).
* Delete a stale auction, or a liquidation against your own position.
* Settle a liquidated position's residual bad debt and orphan its collateral
  ([pool/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/contract.rs#L593-L597)).
* Trigger the pool's status recomputation, and call the oracle's `refresh`.
