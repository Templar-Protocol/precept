# Precept

Precept is Templar's fork of [Blend v2](https://github.com/blend-capital/blend-contracts-v2), a lending protocol
for Soroban. A Precept pool keeps Blend's pool mechanics unchanged — b-dollars and d-dollars, interest accrued on
demand through utilization curves, liquidation auctions filled against live pool state. What differs is who absorbs a
loss: Precept runs with the backstop disabled and several entrypoints switched off, so a defaulted position is
socialised to the lenders of the affected reserve. The backstop, the auction book and the price oracle all remain in
the pool, because the code paths need them — the backstop does not.

This documentation describes the fork. For the mechanics that are unchanged — the b-dollar/d-dollar accounting, the
interest curve, the auction mechanics — read the upstream [Blend documentation](https://docs.blend.capital) instead
of a second copy of the same math.

## What is different, in four points

* The backstop cannot fund a liquidation and cannot absorb bad debt. Its insurance and rewards are switched off, and
  the assets it could ever hold are fixed by its constructor.
* Bad debt is socialised pro rata to the b-dollar holders of the affected reserve — the lenders who are actually
  exposed to that asset.
* The only auction type is user liquidation. Interest and bad-debt auctions are switched off, so interest reaches
  lenders directly.
* The backstop's share of interest (`bstop_rate`) is fixed at 0 at deploy, so the backstop takes no interest margin.

## Status

* Releases are published on [GitHub](https://github.com/Templar-Protocol/precept/releases) with per-file checksums.
  The Guardian review of this fork underlies v0.1.0; v0.1.1 is the Guardian-reviewed v0.1.0 source plus small
  post-review changes, rebuilt on a newer toolchain (see [Security and audits](security.md)).
* **No official Precept market is live yet.** Until one is published at
  [Official markets](official-markets.md), no contract address should be trusted from anywhere else.

## Pages

* [How Precept works](how-it-works.md) — the flow from an isolated user to a socialised loss, with the pool's
  levers.
* [Lending economics](lending-economics.md) — what a lender is exposed to, where a loss lands, and how the oracle
  and interest rate are constructed.
* [Risks](risks.md) — what can go wrong and who can act: every admin and governance lever that affects a pool.
* [Official markets](official-markets.md) — the official mainnet markets, and the addresses to trust.
* [Changes from Blend v2](changes-from-blend-v2.md) — a stock-versus-Precept table, with a permalink to the code on
  both sides of every row.
* [Security and audits](security.md) — the Guardian review, the upstream audits, how to reproduce a release build,
  and how to report a vulnerability.
