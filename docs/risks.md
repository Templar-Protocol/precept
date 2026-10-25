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
| The oracle stops publishing | Any operation that reads a price traps — a borrow, a collateral or same-reserve withdrawal by an account that still owes, and opening or clearing a liquidation auction | The oracle's own governance and its source set |
| The pool admin freezes the market | Status 4 stops new borrows, supplies and cancellations; withdrawals, repayments and auction fills continue | Nothing — the decision is one-way |
| The backstop's position moves sharply | Automatic on-ice/freeze statuses depend on the backstop's balance and on how much of it is queued for withdrawal | Nothing forces the backstop to stay |
| An asset is disabled | Deposits, collateral deposits and borrows of that one asset stop | The pool admin, who can disable it |
| Direct token transfers to the pool | They are not credited to lenders or to the backstop and are not recoverable | Nothing — this is the documented policy |

The last row is worth reading twice: the pool's balance reconciliation is accounting-based
([pool/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/contract.rs#L206-L216)),
so sending a token directly to a pool contract does not make its lenders richer.

## The oracle is the single point of failure

A health check prices the acting account's own positions, and it runs only when that account has liabilities at all
([pool/src/pool/submit.rs:214-219](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/submit.rs#L214-L219)).
The requests that ask for one are borrow
([pool/src/pool/actions.rs:494-511](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L494-L511)),
withdrawing collateral
([pool/src/pool/actions.rs:467-484](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L467-L484)),
a plain withdrawal from a reserve in which the withdrawer also has debt
([pool/src/pool/actions.rs:406-408](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L406-L408)),
and filling or deleting a user-liquidation auction — where the positions priced are the filler's or deleter's own,
not the borrower's
([pool/src/pool/actions.rs:253-262](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L253-L262),
[pool/src/pool/actions.rs:313-316](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L313-L316)).
A fill that clears the auction also values the borrower's remaining collateral before anything is written off
([pool/src/auctions/user_liquidation_auction.rs:218-220](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/auctions/user_liquidation_auction.rs#L218-L220),
[pool/src/pool/bad_debt.rs:63-74](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/bad_debt.rs#L63-L74)).
Opening a liquidation auction prices the borrower's position directly
([pool/src/pool/health_factor.rs:27-42](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/health_factor.rs#L27-L42),
[pool/src/auctions/user_liquidation_auction.rs:47](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/auctions/user_liquidation_auction.rs#L47)).
Supplying collateral does not check health; it loads one price only when a borrower who already has debt opens a
collateral position in a reserve they have not used as collateral before
([pool/src/pool/actions.rs:445-447](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/actions.rs#L445-L447)).
A price is accepted only if the
oracle answers, is not older than 24 hours, is not dated in the future, is positive, and reports 7 decimals
([pool/src/pool/pool.rs:123-143](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/pool.rs#L123-L143),
[pool/src/pool/pool.rs:100-113](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/pool.rs#L100-L113));
any failure traps with `InvalidPrice`, and a missing price traps rather than returning a default. Prices are read only
when a reserve is actually used by an operation, so a reserve can be configured with a feed that has never been
exercised
([pool/src/pool/health_factor.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/health_factor.rs#L35-L42)).
When a price is refused, lending and borrowing for that market stop; a lender who owes nothing in the reserve they are
withdrawing from can still exit, because a plain withdrawal reads no price unless the withdrawer has debt in that reserve.

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
| `queue_set_reserve` / `cancel_set_reserve` | Queue switching an existing reserve off, or cancel a queued change before it is applied | The queued change waits one week before it can be applied ([pool/src/pool/config.rs:94-98](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/config.rs#L94-L98)). The admin can cancel it at any time before it is applied: the cancel is an unconditional delete, with no check on the unlock time or the pool status ([pool/src/pool/config.rs:110-112](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/config.rs#L110-L112)), and it needs only the admin's authorisation ([pool/src/contract.rs:420-428](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/contract.rs#L420-L428)). **Any account** can apply it with `set_reserve` once the week has passed ([pool/src/contract.rs:430-437](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/contract.rs#L430-L437)). After activation that is the only change an existing reserve can undergo: no parameter of a live reserve can be edited, including its supply cap, a disabled reserve cannot be re-enabled, and no new reserve can be added ([pool/src/pool/config.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/config.rs#L85-L88), [pool/src/pool/config.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/config.rs#L115-L134)) |
| `propose_admin` / `accept_admin` | Hand the pool to another admin | Two signatures required: the current admin proposes and the new admin accepts. The handover takes effect immediately when `accept_admin` is called, with no delay ([pool/src/contract.rs:383-404](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/contract.rs#L383-L404)) |
| Configuration before activation | While a pool is still in its `setup` status, the admin can add reserves and set their parameters (within the reserve-metadata bounds) with no timelock: a queued change unlocks at once | The window closes when the admin moves the pool out of `setup` with `set_status` ([pool/src/pool/config.rs:85-98](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/config.rs#L85-L98), [pool/src/pool/status.rs:84-141](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/pool/status.rs#L84-L141)) |

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
underwater position can still be cleared. Only the pool admin can queue a change to the flag, and after activation only in
the disable direction, a week later — an existing reserve can be turned off, and a reserve that is off cannot be turned back on
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
| Change the sources configured for an asset, or an asset's maximum accepted age | A `set_proxy` proposal, created and executed by the proxy-configuration role (or an admin); it waits out its time-to-live first | [engine.rs:75-83](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/governance-contract/src/engine.rs#L75-L83), [roles per action: governance-common/src/lib.rs:170-197](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/governance-common/src/lib.rs#L170-L197) |
| Remove an asset from the proxy | A `remove_proxy` proposal de-lists the asset and deletes its configuration, breaker and price-history state, so the proxy has no price to give for it | [engine.rs:84-89](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/governance-contract/src/engine.rs#L84-L89), [contract/src/lib.rs:194-205](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/contract/src/lib.rs#L194-L205) |
| Configure, add, remove, re-arm or change the enforcement of a circuit breaker | Proposals of the same kind; the first three need the proxy-configuration role, re-arm and enforcement the breaker-operator role (an admin can do any of them) | [engine.rs:90-145](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/governance-contract/src/engine.rs#L90-L145) |
| Halt an asset's price | A manual trip on one asset, which also waits for its proposal time-to-live before it is executed | [engine.rs:146-160](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/governance-contract/src/engine.rs#L146-L160) |
| Replace the runtime contract's code | An `upgrade` proposal, which needs an admin; the runtime accepts it only from its owner | [engine.rs:208-219](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/governance-contract/src/engine.rs#L208-L219), [contract/src/lib.rs:138-163](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/contract/src/lib.rs#L138-L163) |
| Transfer or renounce ownership of the runtime contract | Two-step transfer, accept and renounce proposals, which need an admin | [engine.rs:161-188](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/governance-contract/src/engine.rs#L161-L188), [contract/src/lib.rs:407-413](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/contract/src/lib.rs#L407-L413) |
| Change how long a kind of proposal waits | A `set_action_ttl` proposal; an admin for admin-only kinds, the proxy-configuration role otherwise | [engine.rs:189-199](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/governance-contract/src/engine.rs#L189-L199), [governance-common/src/lib.rs:190-197](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/governance-common/src/lib.rs#L190-L197) |
| Appoint or revoke governance roles, including admin | A `set_role` proposal, which needs an admin | [engine.rs:200-207](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/governance-contract/src/engine.rs#L200-L207), [roles.rs:27-49](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/governance-contract/src/roles.rs#L27-L49) |

Who holds those roles is not fixed. Admin is a role with any number of members, not a single address: the account given
when the governance contract is initialised starts as the only admin
([contract/proxy-oracle/soroban/governance-contract/src/lib.rs:43-68](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/governance-contract/src/lib.rs#L43-L68)),
but admin membership is itself a governance action: an admin can grant admin to another account or revoke it, and the
only guard is that the last admin cannot be removed
([contract/proxy-oracle/soroban/governance-contract/src/roles.rs:31-49](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/governance-contract/src/roles.rs#L31-L49)).
An admin also passes the role check for every other action
([contract/proxy-oracle/soroban/governance-contract/src/engine.rs:30-42](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/governance-contract/src/engine.rs#L30-L42)).

The governed arrangement in the table applies only while the runtime contract's owner is the governance contract. The
owner is whatever address the runtime contract's constructor was given — a deployment choice, not something the contract
enforces
([contract/proxy-oracle/soroban/contract/src/lib.rs:129-136](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/contract/src/lib.rs#L129-L136)) —
and ownership can later be transferred or renounced
([contract/proxy-oracle/soroban/contract/src/lib.rs:407-413](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/contract/src/lib.rs#L407-L413)).
If the owner is a plain account, that account can call `set_proxy`, `set_manual_trip` and `upgrade` directly, with no
proposal and no waiting time
([contract/proxy-oracle/soroban/contract/src/lib.rs:138-163](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/contract/src/lib.rs#L138-L163),
[contract/proxy-oracle/soroban/contract/src/lib.rs:165-192](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/contract/src/lib.rs#L165-L192),
[contract/proxy-oracle/soroban/contract/src/lib.rs:327-360](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/contract/src/lib.rs#L327-L360)).

The waiting time is a parameter, not a fixed floor. Every kind of proposal starts with the same time-to-live, set when
the governance contract is initialised
([contract/proxy-oracle/soroban/governance-contract/src/lib.rs:43-68](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/governance-contract/src/lib.rs#L43-L68)),
and each kind can then be changed through a `set_action_ttl` proposal
([contract/proxy-oracle/soroban/governance-contract/src/engine.rs:189-199](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/governance-contract/src/engine.rs#L189-L199)).
A proposal waits for the larger of its kind's configured time-to-live and the one its proposer asks for, up to a
180-day ceiling
([contract/proxy-oracle/soroban/governance-contract/src/engine.rs:44-59](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/governance-contract/src/engine.rs#L44-L59),
[contract/proxy-oracle/soroban/governance-common/src/lib.rs:11-12](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/governance-common/src/lib.rs#L11-L12)),
and the contract refuses to execute it before that time has passed
([contract/proxy-oracle/governance-kernel/src/lib.rs:452-477](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/governance-kernel/src/lib.rs#L452-L477)).
There is no minimum: the only check on a new time-to-live is the ceiling
([contract/proxy-oracle/soroban/governance-common/src/lib.rs:258-260](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/governance-common/src/lib.rs#L258-L260)),
so it can be set to zero, and a proposal with a zero time-to-live can be executed as soon as it exists
([contract/proxy-oracle/governance-kernel/src/lib.rs:44-46](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/governance-kernel/src/lib.rs#L44-L46)).
Lowering a time-to-live is itself delayed by the longer of the set-time-to-live proposal's own wait and the target's
([contract/proxy-oracle/soroban/governance-common/src/lib.rs:223-232](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/governance-common/src/lib.rs#L223-L232)).
Actions are authorised against a single required role per action type (or the admin), and the same role check applies
to creating, cancelling and executing a proposal, so the split between who may propose and who may execute is a
configuration choice, not a property of the contract
([contract/proxy-oracle/soroban/governance-contract/src/engine.rs:30-42](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/governance-contract/src/engine.rs#L30-L42),
[contract/proxy-oracle/soroban/governance-contract/src/lib.rs:94-167](https://github.com/Templar-Protocol/contracts/blob/df751d48878f7ce1e1bcefd8ce33ae93cd9f1269/contract/proxy-oracle/soroban/governance-contract/src/lib.rs#L94-L167)).

What no one can change about a live market: the pool's oracle address is fixed at deployment, and the 7-decimal price
format is required by the pool's own checks.

## What nobody can change

The pool, the backstop and the pool factory have no upgrade entrypoint — there is no function in any of the three that
replaces their compiled code, so a latent bug found after a market is live stays in that market for its whole life,
and changing what one of these contracts does takes a new deployment and moving users to it. Oracle behaviour is
governed separately, and it is a real lever over how a live market prices — see [Oracle governance](#oracle-governance)
above.

## What anyone can do

Precept has no privileged liquidator. These are open to any account:

* Open a user-liquidation auction for an under-collateralised position, and fill auctions
  ([pool/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/contract.rs#L565-L591)).
* Delete a stale auction, or a liquidation against your own position.
* Settle a liquidated position's residual bad debt and orphan its collateral
  ([pool/src/contract.rs](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/contract.rs#L593-L597)).
* Trigger the pool's status recomputation, and call the oracle's `refresh`.
* Apply a queued reserve change once its one-week timelock has passed, with `set_reserve`
  ([pool/src/contract.rs:430-437](https://github.com/Templar-Protocol/precept/blob/62c8e9bee072d5e58494c0f7a7bf777926e7ea2b/pool/src/contract.rs#L430-L437)).
