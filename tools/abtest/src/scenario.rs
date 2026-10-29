//! Scenario definitions: data only. Ported from
//! `blend-contracts-v2/test-suites/src/differential/scenarios.rs` with testnet
//! substitutions (no ledger jumps; waits and oracle reprices instead).

use crate::manifest::{Tok, Who};
use std::collections::BTreeMap;

pub const SCALAR_7: i128 = 10_000_000;
pub const E6: i128 = 1_000_000;
pub const E9: i128 = 1_000_000_000;
pub const I128_MAX: &str = "170141183460469231731687303715884105727";

/// Pool `Request.request_type` codes (`pool/src/pool/actions.rs`).
pub mod req {
    pub const SUPPLY: u32 = 0;
    pub const WITHDRAW: u32 = 1;
    pub const SUPPLY_COLLATERAL: u32 = 2;
    pub const WITHDRAW_COLLATERAL: u32 = 3;
    pub const BORROW: u32 = 4;
    pub const REPAY: u32 = 5;
    pub const FILL_USER_LIQ: u32 = 6;
}

/// The prices `deploy` writes, in `Tok::ORACLE_ORDER`. Named so that every reset in the
/// strict-oracle probes restores exactly what the rest of the run expects to find.
pub const GOOD_PRICES: [i128; 4] = [2_000 * SCALAR_7, SCALAR_7, 1_000_000, SCALAR_7];

/// Contract error codes the scenarios expect.
pub mod err {
    pub const BACKSTOP_BAD_REQUEST: u32 = 1000;
    pub const POOL_BAD_REQUEST: u32 = 1200;
    pub const POOL_INVALID_RESERVE_METADATA: u32 = 1202;
    pub const POOL_STATUS_NOT_ALLOWED: u32 = 1204;
    pub const POOL_INVALID_STATUS: u32 = 1206;
    pub const POOL_INVALID_PRICE: u32 = 1210;
}

#[derive(Clone, Debug, PartialEq)]
pub struct Req {
    pub kind: u32,
    pub asset: Tok,
    /// Decimal string; may be `I128_MAX`.
    pub amount: String,
}

impl Req {
    pub fn new(kind: u32, asset: Tok, amount: i128) -> Self {
        Self {
            kind,
            asset,
            amount: amount.to_string(),
        }
    }
    pub fn max(kind: u32, asset: Tok) -> Self {
        Self {
            kind,
            asset,
            amount: I128_MAX.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrapCall {
    BackstopDistribute,
    BackstopGulpEmissions,
    BackstopAddReward,
    BackstopRemoveReward,
    BackstopClaim,
    BackstopDrop,
    PoolUpdatePool,
    PoolFlashLoan,
    PoolSetEmissionsConfig,
}

impl TrapCall {
    pub const ALL: [TrapCall; 9] = [
        TrapCall::BackstopDistribute,
        TrapCall::BackstopGulpEmissions,
        TrapCall::BackstopAddReward,
        TrapCall::BackstopRemoveReward,
        TrapCall::BackstopClaim,
        TrapCall::BackstopDrop,
        TrapCall::PoolUpdatePool,
        TrapCall::PoolFlashLoan,
        TrapCall::PoolSetEmissionsConfig,
    ];
    pub fn label(self) -> &'static str {
        match self {
            TrapCall::BackstopDistribute => "trap_backstop_distribute",
            TrapCall::BackstopGulpEmissions => "trap_backstop_gulp_emissions",
            TrapCall::BackstopAddReward => "trap_backstop_add_reward",
            TrapCall::BackstopRemoveReward => "trap_backstop_remove_reward",
            TrapCall::BackstopClaim => "trap_backstop_claim",
            TrapCall::BackstopDrop => "trap_backstop_drop",
            TrapCall::PoolUpdatePool => "trap_pool_update_pool",
            TrapCall::PoolFlashLoan => "trap_pool_flash_loan",
            TrapCall::PoolSetEmissionsConfig => "trap_pool_set_emissions_config",
        }
    }
    /// Error the fork must return.
    pub fn fork_error(self) -> u32 {
        match self {
            TrapCall::PoolUpdatePool
            | TrapCall::PoolFlashLoan
            | TrapCall::PoolSetEmissionsConfig => err::POOL_BAD_REQUEST,
            _ => err::BACKSTOP_BAD_REQUEST,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    Mint {
        token: Tok,
        to: Who,
        amount: i128,
    },
    LpTransfer {
        from: Who,
        to: Who,
        amount: i128,
    },
    Submit {
        from: Who,
        reqs: Vec<Req>,
    },
    /// Full repay of `asset` computed per side from `get_reserve` (d_rate) at run time.
    RepayAll {
        from: Who,
        asset: Tok,
    },
    FillAuction {
        filler: Who,
        victim: Who,
        percent: u32,
    },
    /// Prices in `Tok::ORACLE_ORDER`, 7 decimals.
    SetPrices(Vec<i128>),
    /// The same prices, stamped `offset_secs` from now, so the observation can be made
    /// future-dated or stale on purpose. ADR 0008 restriction 5 is the only way to reach
    /// those branches: the mock oracle otherwise always writes the present.
    SetPricesAt {
        prices: Vec<i128>,
        offset_secs: i64,
    },
    SetStatus(u32),
    UpdateStatus,
    BackstopDeposit {
        who: Who,
        amount: i128,
    },
    /// Fraction of the depositor's shares, in percent.
    BackstopQueuePct {
        who: Who,
        pct: u32,
    },
    BackstopDequeueHalf {
        who: Who,
    },
    BackstopWithdrawQueued {
        who: Who,
    },
    NewAuction {
        user: Who,
        bid: Vec<Tok>,
        lot: Vec<Tok>,
        percent: u32,
    },
    DelAuction {
        user: Who,
    },
    BadDebt {
        user: Who,
    },
    Gulp {
        asset: Tok,
    },
    /// Admin queues a reserve change: `modify` an existing reserve's c_factor, or add a new one.
    QueueSetReserve {
        asset: Tok,
        modify: bool,
    },
    Trap(TrapCall),
    /// Wait until `latest_ledger >= auction_block(auction_of) + min_delta`.
    WaitLedgersSince {
        auction_of: Who,
        min_delta: u32,
    },
    /// Poll `del_auction` simulation until it stops returning `code`, then real call.
    DelAuctionWhenStale {
        user: Who,
        code: u32,
    },
    WaitSeconds(u64),
    /// Reads only; used to assert rates increased (per side, run-time check).
    AssertRatesIncreased {
        asset: Tok,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expect {
    Ok,
    Err(u32),
    /// Any error is fine (code differs between sides or is unspecified).
    AnyErr,
    /// Profile decides (trap steps, bad-debt divergence).
    PerSide,
    /// Record only; both sides are still compared.
    Any,
}

#[derive(Clone, Debug)]
pub struct LabeledStep {
    pub label: &'static str,
    pub step: Step,
    pub expect: Expect,
    /// Only this step's numeric snapshot leaves depend on inclusion-ledger deltas.
    pub delta_dependent: bool,
}

fn ok(label: &'static str, step: Step) -> LabeledStep {
    LabeledStep {
        label,
        step,
        expect: Expect::Ok,
        delta_dependent: false,
    }
}
fn errs(label: &'static str, step: Step, code: u32) -> LabeledStep {
    LabeledStep {
        label,
        step,
        expect: Expect::Err(code),
        delta_dependent: false,
    }
}
fn anyerr(label: &'static str, step: Step) -> LabeledStep {
    LabeledStep {
        label,
        step,
        expect: Expect::AnyErr,
        delta_dependent: false,
    }
}
fn per_side(label: &'static str, step: Step) -> LabeledStep {
    LabeledStep {
        label,
        step,
        expect: Expect::PerSide,
        delta_dependent: false,
    }
}
fn any(label: &'static str, step: Step) -> LabeledStep {
    LabeledStep {
        label,
        step,
        expect: Expect::Any,
        delta_dependent: false,
    }
}
fn delta(mut s: LabeledStep) -> LabeledStep {
    s.delta_dependent = true;
    s
}

/// What the fork must do on a `PerSide` step (stock is recorded, not judged).
pub fn fork_expect(label: &str, step: &Step) -> Expect {
    match step {
        Step::Trap(t) => Expect::Err(t.fork_error()),
        Step::QueueSetReserve { .. } => Expect::Err(err::POOL_INVALID_RESERVE_METADATA),
        Step::Gulp { .. } if label.contains("with_debt") => Expect::Err(err::POOL_BAD_REQUEST),
        // Stock rejects stale and nonpositive observations already; the future-dated
        // branch is what restriction 5 adds, so only the fork refuses this one.
        _ if label.contains("future_price") => Expect::Err(err::POOL_INVALID_PRICE),
        // The frozen-absorbing guard rejecting a downgrade is the fork's addition.
        Step::SetStatus(0) if label == FREEZE_DOWNGRADE_PROBE => {
            Expect::Err(err::POOL_STATUS_NOT_ALLOWED)
        }
        _ => Expect::Ok,
    }
}
/// The probe that downgrades the stock pool from frozen (4) to normal (0),
/// which the fork's absorbing guard refuses. Once it has executed, the
/// persistent 0-vs-4 `config.status` spread is a sanctioned divergence.
pub const FREEZE_DOWNGRADE_PROBE: &str = "freeze_set_status_0_rejected";

/// The full labeled spec for a label, borrowed from a once-built index (first
/// declaration wins, matching the historical `find` scans) so per-step
/// comparison and replay never rebuild or clone scenario data.
static STEP_INDEX: std::sync::LazyLock<BTreeMap<&'static str, LabeledStep>> =
    std::sync::LazyLock::new(|| {
        let mut idx = BTreeMap::new();
        for s in all_steps() {
            idx.entry(s.label).or_insert(s);
        }
        idx
    });

pub fn current_step(label: &str) -> Option<&'static LabeledStep> {
    STEP_INDEX.get(label)
}

/// The isolated scenario families; each invocation runs one on its own fresh
/// deployment. Every table must stay a subsequence of `scenarios()`
/// declaration order — that order is the canonical spelling.
const FAMILIES: &[&[&str]] = &[
    &[
        "lending", "interest", "statuses", "backstop", "stress", "freeze",
    ],
    &["strict_oracle"],
    &["liquidation", "stale_auction", "direct_default"],
];

/// The family these names belong to, when they are distinct members of
/// exactly one family. Order-free; empty input, duplicates, or cross-family
/// mixing have no family.
pub fn family_of(names: &[&str]) -> Option<&'static [&'static str]> {
    let mut uniq = names.to_vec();
    uniq.sort_unstable();
    uniq.dedup();
    if uniq.is_empty() || uniq.len() != names.len() {
        return None;
    }
    FAMILIES
        .iter()
        .copied()
        .find(|f| uniq.iter().all(|n| f.contains(n)))
}

/// The loss family's scope rule: `stale_auction` needs `liquidation` in the
/// same scope; `direct_default` needs both. One authority shared by
/// drive-time selection and replay-time scope integrity.
pub fn loss_prerequisites_ok(names: &[&str]) -> bool {
    (!names.contains(&"stale_auction") || names.contains(&"liquidation"))
        && (!names.contains(&"direct_default")
            || (names.contains(&"liquidation") && names.contains(&"stale_auction")))
}

/// Every invocation must use one isolated family on its own fresh deployment.
pub const SELECTION_GUIDANCE: &str = "select a canonical comma-separated subset of one family: lending,interest,statuses,backstop,stress,freeze OR strict_oracle OR liquidation,stale_auction,direct_default; use a fresh deployment for each family";

pub fn select_scenarios(selection: Option<&str>) -> anyhow::Result<Vec<Scenario>> {
    let selection = selection.ok_or_else(|| anyhow::anyhow!(SELECTION_GUIDANCE))?;
    let names: Vec<_> = selection.split(',').collect();
    let family = family_of(&names).ok_or_else(|| {
        anyhow::anyhow!("invalid scenario selection {selection:?}; {SELECTION_GUIDANCE}")
    })?;
    let canonical: Vec<_> = family
        .iter()
        .copied()
        .filter(|n| names.contains(n))
        .collect();
    anyhow::ensure!(
        names == canonical,
        "duplicate or noncanonical scenario selection; {SELECTION_GUIDANCE}"
    );
    anyhow::ensure!(
        loss_prerequisites_ok(&names),
        "loss-family prerequisites incomplete (stale_auction requires liquidation; direct_default requires liquidation,stale_auction); {SELECTION_GUIDANCE}"
    );
    Ok(scenarios()
        .into_iter()
        .filter(|s| names.contains(&s.name))
        .collect())
}

fn all_steps() -> Vec<LabeledStep> {
    scenarios()
        .into_iter()
        .flat_map(|s| s.steps)
        .chain(traps())
        .collect()
}

/// Deterministic pseudo-random mixed operations (seeded LCG) to stress invariants.
pub fn stress_steps(seed: u64, n: usize) -> Vec<LabeledStep> {
    use req::*;
    let mut x = seed;
    let mut next = move |m: usize| -> usize {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((x >> 33) as usize) % m
    };
    let actors = [Who::Sam, Who::User, Who::Frodo];
    let amounts: [(Tok, [i128; 3]); 3] = [
        (Tok::Stable, [100 * E6, 500 * E6, 2_000 * E6]),
        (
            Tok::Xlm,
            [1_000 * SCALAR_7, 10_000 * SCALAR_7, 50_000 * SCALAR_7],
        ),
        (Tok::Weth, [E9 / 2, E9, 5 * E9]),
    ];
    let prices: [[i128; 4]; 3] = [
        [2_000 * SCALAR_7, SCALAR_7, 1_000_000, SCALAR_7],
        [1_800 * SCALAR_7, SCALAR_7, 900_000, SCALAR_7],
        [2_200 * SCALAR_7, SCALAR_7, 1_100_000, SCALAR_7],
    ];
    let mut steps = vec![ok(
        "stress_reset_prices",
        Step::SetPrices(prices[0].to_vec()),
    )];
    for who in [Who::Sam, Who::User] {
        for (tok, amts) in &amounts {
            let label: &'static str = Box::leak(
                format!("stress_mint_{}_{}", who.as_str(), tok.as_str()).into_boxed_str(),
            );
            steps.push(ok(
                label,
                Step::Mint {
                    token: *tok,
                    to: who,
                    amount: amts[2] * 4,
                },
            ));
        }
    }
    for i in 0..n {
        let label: &'static str = Box::leak(format!("stress_{i:03}").into_boxed_str());
        if i % 7 == 6 {
            steps.push(any(label, Step::SetPrices(prices[next(3)].to_vec())));
            continue;
        }
        let who = actors[next(3)];
        let (tok, amts) = &amounts[next(3)];
        let amount = amts[next(3)];
        let kind = [
            SUPPLY_COLLATERAL,
            BORROW,
            REPAY,
            WITHDRAW_COLLATERAL,
            SUPPLY,
            WITHDRAW,
        ][next(6)];
        steps.push(any(
            label,
            Step::Submit {
                from: who,
                reqs: vec![Req::new(kind, *tok, amount)],
            },
        ));
    }
    steps
}

pub struct Scenario {
    pub name: &'static str,
    /// What this experiment demonstrates, and what a divergence here would mean.
    pub description: &'static str,
    pub steps: Vec<LabeledStep>,
}

/// The description for a scenario name, for evidence loaded from an older run whose
/// scenario list no longer matches the code.
pub fn describe(name: &str) -> &'static str {
    scenarios()
        .into_iter()
        .find(|s| s.name == name)
        .map(|s| s.description)
        .unwrap_or("")
}

pub fn traps() -> Vec<LabeledStep> {
    TrapCall::ALL
        .iter()
        .map(|t| per_side(t.label(), Step::Trap(*t)))
        .collect()
}

pub fn scenarios() -> Vec<Scenario> {
    use req::*;
    use Step::*;
    use Tok::*;
    use Who::*;
    vec![
        Scenario {
            name: "lending",
            description: "Ordinary lending end to end: supply collateral, borrow, repay in part and in full, supply, withdraw everything. Nothing here is forked, so the two pools must agree exactly; any divergence means the fork changed a path it was not supposed to touch.",
            steps: vec![
                ok(
                    "lending_mint_xlm",
                    Mint {
                        token: Xlm,
                        to: User,
                        amount: 160_000 * SCALAR_7,
                    },
                ),
                ok(
                    "lending_mint_stable",
                    Mint {
                        token: Stable,
                        to: User,
                        amount: 6_000 * E6,
                    },
                ),
                ok(
                    "supply_collateral",
                    Submit {
                        from: User,
                        reqs: vec![Req::new(SUPPLY_COLLATERAL, Xlm, 160_000 * SCALAR_7)],
                    },
                ),
                ok(
                    "borrow",
                    Submit {
                        from: User,
                        reqs: vec![Req::new(BORROW, Stable, 400 * E6)],
                    },
                ),
                ok(
                    "partial_repay",
                    Submit {
                        from: User,
                        reqs: vec![Req::new(REPAY, Stable, 250 * E6)],
                    },
                ),
                ok(
                    "full_repay",
                    RepayAll {
                        from: User,
                        asset: Stable,
                    },
                ),
                ok(
                    "supply",
                    Submit {
                        from: User,
                        reqs: vec![Req::new(SUPPLY, Stable, 2_000 * E6)],
                    },
                ),
                ok(
                    "withdraw_max",
                    Submit {
                        from: User,
                        reqs: vec![Req::max(WITHDRAW, Stable)],
                    },
                ),
                ok(
                    "withdraw_collateral_max",
                    Submit {
                        from: User,
                        reqs: vec![Req::max(WITHDRAW_COLLATERAL, Xlm)],
                    },
                ),
            ],
        },
        Scenario {
            name: "interest",
            description: "Let real time pass and confirm interest accrues: b_rate and d_rate must both increase on each side. Establishes that the rate machinery is untouched before later scenarios depend on it.",
            steps: vec![
                ok("interest_wait", WaitSeconds(120)),
                ok(
                    "interest_supply",
                    Submit {
                        from: Frodo,
                        reqs: vec![Req::new(SUPPLY, Stable, 100 * E6)],
                    },
                ),
                ok(
                    "interest_rates_increased",
                    AssertRatesIncreased { asset: Stable },
                ),
            ],
        },
        Scenario {
            name: "statuses",
            description: "Admin status transitions, plus the sealed-configuration probes. On the fork a reserve may only go enabled -> disabled, so both queueing a parameter change and adding a new reserve must be rejected.",
            steps: vec![
                ok("status_admin_on_ice", SetStatus(3)),
                ok("status_update_back_to_active", UpdateStatus),
                per_side(
                    "sealed_queue_modify",
                    QueueSetReserve {
                        asset: Xlm,
                        modify: true,
                    },
                ),
                per_side(
                    "sealed_queue_new",
                    QueueSetReserve {
                        asset: Blnd,
                        modify: false,
                    },
                ),
            ],
        },
        Scenario {
            name: "strict_oracle",
            description: "ADR 0008 restriction 5: the pool refuses an observation it cannot trust. Stock Blend already rejects a stale or nonpositive price, so those two probes must agree on both sides and prove no regression. The future-dated branch is the one the fork adds, so there the stock pool borrows happily and the fork must reject with InvalidPrice; a fork that accepts it has lost the restriction. Every probe restores good prices afterwards, so nothing downstream inherits a poisoned oracle.",
            steps: vec![
                ok(
                    "oracle_mint_frodo",
                    Mint {
                        token: Stable,
                        to: Frodo,
                        amount: 5_000 * E6,
                    },
                ),
                ok(
                    "oracle_frodo_collateral",
                    Submit {
                        from: Frodo,
                        reqs: vec![Req::new(SUPPLY_COLLATERAL, Stable, 3_000 * E6)],
                    },
                ),
                ok("oracle_good_prices", SetPrices(GOOD_PRICES.to_vec())),
                // Baseline: the same borrow both sides accept, so a later rejection is
                // attributable to the observation and nothing else.
                ok(
                    "oracle_baseline_borrow",
                    Submit {
                        from: Frodo,
                        reqs: vec![Req::new(BORROW, Xlm, 10 * SCALAR_7)],
                    },
                ),
                // 1. future-dated: the branch restriction 5 adds. Stock has no such check.
                ok(
                    "oracle_set_future_price",
                    SetPricesAt {
                        prices: GOOD_PRICES.to_vec(),
                        offset_secs: 3_600,
                    },
                ),
                per_side(
                    "oracle_borrow_future_price",
                    Submit {
                        from: Frodo,
                        reqs: vec![Req::new(BORROW, Xlm, 10 * SCALAR_7)],
                    },
                ),
                // Frodo carries 65,000 XLM of deploy-seeded debt plus the
                // baseline borrow, and stock also accepted the future-dated
                // borrow. Repaying all debt on both sides here — repay takes
                // no oracle read and refunds excess (`apply_repay`) — is what
                // stops the reset and later probes inheriting the borrow spread.
                ok("oracle_repay_after_future", RepayAll { from: Frodo, asset: Xlm }),
                ok("oracle_reset_after_future", SetPrices(GOOD_PRICES.to_vec())),
                // 2. stale beyond a day: both sides must already refuse this.
                ok(
                    "oracle_set_stale_price",
                    SetPricesAt {
                        prices: GOOD_PRICES.to_vec(),
                        offset_secs: -90_000,
                    },
                ),
                errs(
                    "oracle_borrow_stale_price",
                    Submit {
                        from: Frodo,
                        reqs: vec![Req::new(BORROW, Xlm, 10 * SCALAR_7)],
                    },
                    err::POOL_INVALID_PRICE,
                ),
                ok("oracle_reset_after_stale", SetPrices(GOOD_PRICES.to_vec())),
                // 3. nonpositive: likewise a stock check, kept here against regression.
                ok(
                    "oracle_set_zero_price",
                    SetPrices(vec![0, SCALAR_7, 1_000_000, SCALAR_7]),
                ),
                errs(
                    "oracle_borrow_zero_price",
                    Submit {
                        from: Frodo,
                        reqs: vec![Req::new(BORROW, Xlm, 10 * SCALAR_7)],
                    },
                    err::POOL_INVALID_PRICE,
                ),
                // 4. the oracle recovers: a good observation borrows again on both sides,
                // proving the rejections above were the observation and not a wedged pool.
                ok("oracle_reset_prices", SetPrices(GOOD_PRICES.to_vec())),
                ok(
                    "oracle_recovered_borrow",
                    Submit {
                        from: Frodo,
                        reqs: vec![Req::new(BORROW, Xlm, 10 * SCALAR_7)],
                    },
                ),
                ok("oracle_repay_all", RepayAll { from: Frodo, asset: Xlm }),
            ],
        },
        Scenario {
            name: "backstop",
            description: "Backstop deposit, queue and dequeue stay base-equivalent under ADR 0011: only the six emissions exports are trapped, the ordinary share paths are not. Withdrawal is expected to fail, since the lockup cannot elapse during a run.",
            steps: vec![
                ok(
                    "backstop_lp_transfer",
                    LpTransfer {
                        from: Frodo,
                        to: User,
                        amount: 10_000 * SCALAR_7,
                    },
                ),
                ok(
                    "backstop_deposit",
                    BackstopDeposit {
                        who: User,
                        amount: 10_000 * SCALAR_7,
                    },
                ),
                ok(
                    "backstop_queue_30pct",
                    BackstopQueuePct { who: User, pct: 30 },
                ),
                ok("backstop_dequeue_half", BackstopDequeueHalf { who: User }),
                anyerr(
                    "backstop_withdraw_locked",
                    BackstopWithdrawQueued { who: User },
                ),
            ],
        },
        Scenario {
            name: "liquidation",
            description: "A borrower is made unhealthy and liquidated: partial fill, then full fill, then a second auction filled at the zero-bid point where the filler takes all collateral and pays nothing. That last fill is what strips the position and leaves residual debt.",
            steps: vec![
                ok(
                    "liq_frodo_collateral",
                    Submit {
                        from: Frodo,
                        reqs: vec![Req::new(SUPPLY_COLLATERAL, Stable, 30_000 * E6)],
                    },
                ),
                ok(
                    "liq_mint_sam_xlm",
                    Mint {
                        token: Xlm,
                        to: Sam,
                        amount: 500_000 * SCALAR_7,
                    },
                ),
                ok(
                    "liq_mint_sam_weth",
                    Mint {
                        token: Weth,
                        to: Sam,
                        amount: 50 * E9,
                    },
                ),
                ok(
                    "liq_sam_positions",
                    Submit {
                        from: Sam,
                        reqs: vec![
                            Req::new(SUPPLY_COLLATERAL, Xlm, 160_000 * SCALAR_7),
                            Req::new(SUPPLY_COLLATERAL, Weth, 17 * E9),
                            Req::new(BORROW, Stable, 28_000 * E6),
                            Req::new(BORROW, Xlm, 65_000 * SCALAR_7),
                        ],
                    },
                ),
                // Replaces twelve 7-day jumps: make Sam liquidatable by repricing.
                ok(
                    "liq_reprice_down",
                    SetPrices(vec![1_500 * SCALAR_7, SCALAR_7, 700_000, SCALAR_7]),
                ),
                ok(
                    "liq_new_auction_full",
                    NewAuction {
                        user: Sam,
                        bid: vec![Stable, Xlm],
                        lot: vec![Weth, Xlm],
                        percent: 100,
                    },
                ),
                ok(
                    "liq_mint_frodo_stable",
                    Mint {
                        token: Stable,
                        to: Frodo,
                        amount: 100_000 * E6,
                    },
                ),
                ok(
                    "liq_frodo_more_collateral",
                    Submit {
                        from: Frodo,
                        reqs: vec![Req::new(SUPPLY_COLLATERAL, Stable, 100_000 * E6)],
                    },
                ),
                ok(
                    "liq_wait_101",
                    WaitLedgersSince {
                        auction_of: Sam,
                        min_delta: 101,
                    },
                ),
                delta(ok(
                    "liq_fill_25",
                    FillAuction {
                        filler: Frodo,
                        victim: Sam,
                        percent: 25,
                    },
                )),
                delta(ok(
                    "liq_fill_100",
                    FillAuction {
                        filler: Frodo,
                        victim: Sam,
                        percent: 100,
                    },
                )),
                // The full fill above repays the debt and leaves the position flat,
                // so a second auction on it can never be liquidatable — that was
                // the `liq_new_auction_100` 1211 desync in recorded runs. Rebuild
                // the small position the zero-bid auction actually exercises:
                // supply XLM 20k (base 1050 at 0.07) then borrow 1k stable
                // (base 950) — borrowable at the current prices, underwater once
                // the reprice below cuts XLM.
                ok(
                    "liq_repop_mint_sam_xlm",
                    Mint {
                        token: Xlm,
                        to: Sam,
                        amount: 20_000 * SCALAR_7,
                    },
                ),
                ok(
                    "liq_repop_sam_positions",
                    Submit {
                        from: Sam,
                        reqs: vec![
                            Req::new(SUPPLY_COLLATERAL, Xlm, 20_000 * SCALAR_7),
                            Req::new(BORROW, Stable, 1_000 * E6),
                        ],
                    },
                ),
                delta(ok(
                    // Sam keeps the pre-liquidation collateral (~92k XLM incl.
                    // the repop) plus 7.6 WETH: at XLM $0.005 the whole XLM
                    // base is ~458 and WETH (1e-5 dollars) is dust — both far
                    // under the 950 liability base. Prices are dollars x
                    // SCALAR_7: the earlier 800_000 was $0.08, still solvent.
                    // A WETH-only or XLM-only crash both kept rejecting the
                    // auction with 1211 in the recorded runs.
                    "liq_reprice_crash",
                    SetPrices(vec![100, SCALAR_7, 50_000, SCALAR_7]),
                )),
                delta(ok(
                    // Lot covers BOTH collateral reserves: a zero-bid fill on an
                    // Xlm-only lot would strand the 7.67 WETH (worth $15k at the
                    // stale scenario's reset price) and keep every later
                    // "Sam is stripped" scenario healthy at 1211 again.
                    "liq_new_auction_100",
                    NewAuction {
                        user: Sam,
                        bid: vec![Stable],
                        lot: vec![Xlm, Weth],
                        percent: 100,
                    },
                )),
                ok(
                    "liq_wait_zero_bid",
                    WaitLedgersSince {
                        auction_of: Sam,
                        min_delta: 401,
                    },
                ),
                delta(ok(
                    "liq_fill_final",
                    FillAuction {
                        filler: Frodo,
                        victim: Sam,
                        percent: 100,
                    },
                )),
                delta(ok(
                    // The crash prices above would make the alt borrow untenable
                    // (collateral base 600 < residual + new debt). Reset to the
                    // alt block's own baseline: 100k XLM at $0.04 backs a 1k
                    // stable borrow on top of Sam's zero-bid residual.
                    "liq_alt_rebase",
                    SetPrices(vec![2_000 * SCALAR_7, SCALAR_7, 400_000, SCALAR_7]),
                )),
                // Alternate funded filler on a fresh small auction.
                delta(ok(
                    "liq_alt_mint_user_stable",
                    Mint {
                        token: Stable,
                        to: User,
                        // 5k spare balance: the alt fill must PAY its bid, and
                        // the whole mint was previously supplied as collateral.
                        amount: 55_000 * E6,
                    },
                )),
                delta(ok(
                    "liq_alt_user_collateral",
                    Submit {
                        from: User,
                        reqs: vec![Req::new(SUPPLY_COLLATERAL, Stable, 50_000 * E6)],
                    },
                )),
                delta(ok(
                    "liq_alt_mint_sam_xlm",
                    Mint {
                        token: Xlm,
                        to: Sam,
                        amount: 100_000 * SCALAR_7,
                    },
                )),
                delta(ok(
                    "liq_alt_sam_positions",
                    Submit {
                        from: Sam,
                        reqs: vec![
                            Req::new(SUPPLY_COLLATERAL, Xlm, 100_000 * SCALAR_7),
                            Req::new(BORROW, Stable, 1_000 * E6),
                        ],
                    },
                )),
                delta(ok(
                    // Sam still carries the zero-bid residual (1k stable). 100k
                    // XLM at 0.04 gives collateral base 3000 >= post-borrow
                    // liability base 1900 (borrowable); this reprice to 0.01
                    // drops collateral base to 750: liquidatable again.
                    "liq_alt_reprice",
                    SetPrices(vec![500 * SCALAR_7, SCALAR_7, 100_000, SCALAR_7]),
                )),
                delta(ok(
                    "liq_alt_new_auction",
                    NewAuction {
                        user: Sam,
                        bid: vec![Stable],
                        lot: vec![Xlm],
                        percent: 100,
                    },
                )),
                ok(
                    "liq_alt_wait_101",
                    WaitLedgersSince {
                        auction_of: Sam,
                        min_delta: 101,
                    },
                ),
                delta(ok(
                    "liq_alt_fill_user",
                    FillAuction {
                        filler: User,
                        victim: Sam,
                        percent: 100,
                    },
                )),
            ],
        },
        Scenario {
            name: "stale_auction",
            description: "An auction left unfilled for 500 ledgers becomes stale and anyone may delete it. Deleting earlier must be rejected, which is the probe that the staleness rule is intact.",
            steps: vec![
                ok(
                    "stale_reset_prices",
                    SetPrices(vec![2_000 * SCALAR_7, SCALAR_7, 1_000_000, SCALAR_7]),
                ),
                ok(
                    "stale_mint_sam",
                    Mint {
                        token: Stable,
                        to: Sam,
                        amount: 2_500 * E6,
                    },
                ),
                ok(
                    // Sam's state here is accumulated, not fresh: ~465k XLM
                    // collateral against 12k XLM debt (liquidation residue) —
                    // 39x over-collateralized, so no XLM price can make him
                    // unhealthy. The route in is a stable-denominated debt: at
                    // GOOD_PRICES (XLM $0.1) collateral base 2250 + 34875 =
                    // 37125 dwarfs the new 2850 stable liability base.
                    "stale_sam_positions",
                    Submit {
                        from: Sam,
                        reqs: vec![
                            Req::new(SUPPLY_COLLATERAL, Stable, 2_500 * E6),
                            Req::new(BORROW, Xlm, 1_200 * SCALAR_7),
                            Req::new(BORROW, Stable, 3_000 * E6),
                        ],
                    },
                ),
                ok(
                    // XLM -> $0.001 (slot 2 of ORACLE_ORDER [Weth, Usdc, Xlm,
                    // Stable]): collateral base 2250 + 0.75*465000*0.001 =
                    // 2599 < liability base 0.75*12000*0.001 + 0.95*3000 =
                    // 2859. Ratio 1.10 sits inside the full-liquidation
                    // exemption window (1.03, 1.15), so percent 100 is fair.
                    "stale_reprice_unhealthy",
                    SetPrices(vec![2_000 * SCALAR_7, SCALAR_7, 10_000, SCALAR_7]),
                ),
                ok(
                    // Sam's collateral spans stable AND xlm, his liabilities
                    // too, so both lists must cover both reserves.
                    "stale_new_auction_full",
                    NewAuction {
                        user: Sam,
                        bid: vec![Stable, Xlm],
                        lot: vec![Stable, Xlm],
                        percent: 100,
                    },
                ),
                errs(
                    "stale_del_early",
                    DelAuction { user: Sam },
                    err::POOL_BAD_REQUEST,
                ),
                ok(
                    "stale_del_when_stale",
                    DelAuctionWhenStale {
                        user: Sam,
                        code: err::POOL_BAD_REQUEST,
                    },
                ),
            ],
        },
        // Replaces a price-crash setup that could not work: `collateral_raw` is
        // `floor(price * assets / scalar)` per reserve, so driving it to zero needs the
        // position to already be dust, and you cannot borrow against dust. The real route
        // is the zero-bid final fill above: past 400 ledgers the filler takes every unit of
        // collateral and pays nothing, leaving the borrower holding the whole debt with no
        // collateral. That is what makes the fork default directly to suppliers instead of
        // handing the debt to the backstop, which is the divergence this scenario shows.
        Scenario {
            name: "direct_default",
            description: "The divergence this whole tool exists for: a borrower left with debt and no collateral. The fork defaults the loss directly to that reserve's suppliers and emits defaulted_debt; stock hands the debt to the backstop and emits bad_debt. gulp must also refuse to recognise anything while the reserve still has debt.",
            steps: vec![
                ok(
                    "direct_default_reset_prices",
                    SetPrices(vec![2_000 * SCALAR_7, SCALAR_7, 1_000_000, SCALAR_7]),
                ),
                // Sam was stripped by the zero-bid fill: on the fork that already emitted
                // `defaulted_debt` and left the backstop untouched, while stock handed the
                // debt to the backstop. Calling it again is the explicit probe.
                any("direct_default_bad_debt", BadDebt { user: Sam }),
                // `gulp` must refuse to recognise anything while the reserve still has debt.
                per_side("direct_default_gulp_with_debt", Gulp { asset: Xlm }),
            ],
        },
        Scenario {
            name: "stress",
            description: "A seeded pseudo-random mix of supplies, borrows, repays, withdrawals and reprices across every actor and reserve, with the full invariant set checked after each one. Deterministic for a given seed, so a failure reproduces exactly.",
            steps: stress_steps(9_110_008, 36),
        },
        Scenario {
            name: "freeze",
            description: "Status 4 is absorbing. Once set, supply is rejected, update_status and downgrade are rejected, and withdrawal stays open. Frozen repayment is not exercised. Runs last because nothing can follow it.",
            steps: vec![
                ok("freeze_set_status_4", SetStatus(4)),
                errs(
                    "freeze_supply_rejected",
                    Submit {
                        from: Frodo,
                        reqs: vec![Req::new(SUPPLY, Stable, E6)],
                    },
                    err::POOL_INVALID_STATUS,
                ),
                errs(
                    "freeze_update_status_rejected",
                    UpdateStatus,
                    err::POOL_STATUS_NOT_ALLOWED,
                ),
                // Stock accepts the 4 -> 0 downgrade (the frozen-absorbing guard is the
                // fork's addition), so this probe is judged fork-side only; the residual
                // status difference persists by design and compare.rs exempts it.
                per_side(
                    "freeze_set_status_0_rejected",
                    SetStatus(0),
                ),
                ok(
                    "freeze_withdraw_allowed",
                    Submit {
                        from: Frodo,
                        reqs: vec![Req::new(WITHDRAW, Stable, E6)],
                    },
                ),
            ],
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stress_is_deterministic_and_bounded() {
        let a = stress_steps(1, 20);
        let b = stress_steps(1, 20);
        assert_eq!(a.len(), b.len());
        assert!(a
            .iter()
            .zip(&b)
            .all(|(x, y)| x.step == y.step && x.label == y.label));
        assert!(
            a.iter()
                .filter(|s| matches!(s.step, Step::SetPrices(_)))
                .count()
                >= 2
        );
    }

    #[test]
    fn labels_unique_and_freeze_last() {
        let all = scenarios();
        let mut labels: Vec<&str> = all
            .iter()
            .flat_map(|s| s.steps.iter().map(|x| x.label))
            .collect();
        labels.extend(traps().iter().map(|x| x.label));
        let n = labels.len();
        labels.sort();
        labels.dedup();
        assert_eq!(n, labels.len(), "duplicate step labels");
        assert_eq!(all.last().unwrap().name, "freeze");
        for t in traps() {
            assert!(t.label.starts_with("trap_"));
            assert_eq!(t.expect, Expect::PerSide);
        }
    }

    #[test]
    fn selection_is_canonical_isolated_and_complete() {
        // Every scenario is reachable across exactly the three families.
        let covered: Vec<&str> = [
            "lending", "interest", "statuses", "backstop", "stress", "freeze",
        ]
        .into_iter()
        .chain(["strict_oracle"])
        .chain(["liquidation", "stale_auction", "direct_default"])
        .collect();
        let mut names: Vec<_> = scenarios().into_iter().map(|s| s.name).collect();
        names.sort();
        let mut covered = covered;
        covered.sort();
        assert_eq!(names, covered);

        let ok = |s: &str| {
            select_scenarios(Some(s)).map(|v| v.iter().map(|s| s.name).collect::<Vec<_>>())
        };
        assert_eq!(ok("strict_oracle").unwrap(), ["strict_oracle"]);
        assert_eq!(
            ok("liquidation,stale_auction,direct_default").unwrap(),
            ["liquidation", "stale_auction", "direct_default"]
        );
        assert_eq!(
            ok("backstop,stress,freeze").unwrap(),
            ["backstop", "stress", "freeze"]
        );
        for bad in [
            "all",
            "",
            "interest,interest",
            "interest,lending",
            "strict_oracle,lending",
            "stale_auction",
            "liquidation,direct_default",
            "nonsense",
            "freeze,stress,backstop",
        ] {
            assert!(
                select_scenarios(Some(bad)).is_err(),
                "{bad:?} must be rejected"
            );
        }
        assert!(select_scenarios(None).is_err());
    }

    #[test]
    fn family_rules_have_a_single_authority() {
        // Replay-side membership is order-free and prerequisite-free; the
        // loss prereq is a separate shared rule, not a family boundary.
        assert!(family_of(&["stale_auction"]).is_some());
        assert!(!loss_prerequisites_ok(&["stale_auction"]));
        assert!(loss_prerequisites_ok(&["liquidation", "stale_auction"]));
        assert!(family_of(&[]).is_none());
        assert!(family_of(&["interest", "interest"]).is_none());
        assert!(family_of(&["interest", "strict_oracle"]).is_none());
        // The family tables encode canonical order; keep them subsequences
        // of the scenario declaration order or "canonical" drifts silently.
        let declared: Vec<&str> = scenarios().into_iter().map(|s| s.name).collect();
        for family in FAMILIES {
            let mut it = declared.iter();
            assert!(
                family.iter().all(|n| it.any(|d| d == n)),
                "family {family:?} is not a subsequence of {declared:?}"
            );
        }
    }
}

#[cfg(test)]
mod oracle_tests {
    use super::*;

    #[test]
    fn the_strict_oracle_probes_always_restore_a_good_observation() {
        let sc = scenarios()
            .into_iter()
            .find(|s| s.name == "strict_oracle")
            .expect("strict_oracle scenario");
        // A probe that left the oracle future-dated or zeroed would poison every
        // scenario after it, and the failure would look like a protocol bug.
        let mut bad = false;
        for step in &sc.steps {
            match &step.step {
                Step::SetPricesAt { .. } => bad = true,
                Step::SetPrices(p) => bad = p.iter().any(|x| *x <= 0),
                _ => {}
            }
        }
        assert!(!bad, "strict_oracle ends with a poisoned oracle");
        assert!(matches!(
            sc.steps.last().map(|s| &s.step),
            Some(Step::RepayAll { .. })
        ));
        // The stock-only future-price borrow's principal/debt spread must be
        // cleaned up before the stale/zero probes judge anything.
        let future = sc
            .steps
            .iter()
            .position(|s| s.label == "oracle_borrow_future_price")
            .unwrap();
        let stale = sc
            .steps
            .iter()
            .position(|s| s.label == "oracle_set_stale_price")
            .unwrap();
        assert!(sc.steps[future + 1..stale].iter().any(|s| matches!(
            s.step,
            Step::RepayAll {
                from: Who::Frodo,
                asset: Tok::Xlm
            }
        )));
    }

    #[test]
    fn only_the_future_dated_probe_is_a_fork_only_rejection() {
        // Stock Blend rejects stale and nonpositive observations already, so declaring
        // those per-side would hide a real regression behind an expected divergence.
        let sc = scenarios()
            .into_iter()
            .find(|s| s.name == "strict_oracle")
            .unwrap();
        let by = |label: &str| {
            sc.steps
                .iter()
                .find(|s| s.label == label)
                .map(|s| s.expect)
                .unwrap()
        };
        assert_eq!(by("oracle_borrow_future_price"), Expect::PerSide);
        assert_eq!(
            by("oracle_borrow_stale_price"),
            Expect::Err(err::POOL_INVALID_PRICE)
        );
        assert_eq!(
            by("oracle_borrow_zero_price"),
            Expect::Err(err::POOL_INVALID_PRICE)
        );
    }

    #[test]
    fn the_fork_must_refuse_the_future_dated_observation() {
        let sc = scenarios()
            .into_iter()
            .find(|s| s.name == "strict_oracle")
            .unwrap();
        let step = sc
            .steps
            .iter()
            .find(|s| s.label == "oracle_borrow_future_price")
            .unwrap();
        assert_eq!(
            fork_expect(step.label, &step.step),
            Expect::Err(err::POOL_INVALID_PRICE)
        );
    }
}
