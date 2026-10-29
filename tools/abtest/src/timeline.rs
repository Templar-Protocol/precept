//! Run-level accumulator: the views that need more than one step to see.
//!
//! ADR 0008 §8 judges the fork by *changes over time*. Every `b_supply`/`d_supply` move
//! has to be explained by an emitted event, and a move that no event explains is an
//! unexplained accounting change that closes routes. `observe::invariants` only judges a
//! single snapshot, so nothing checked that rule until this module.
//!
//! Everything here is a pure function of step evidence already on disk. No new chain
//! reads: the snapshot cost per step is already the bulk of a run's non-waiting time.

use crate::manifest::{PairManifest, Profile, SideManifest, Tok};
use serde_json::Value;
use std::collections::BTreeMap;

use crate::compare::Class;
use crate::observe::{policy_finding, strict_map, try_num, Snapshot, StepRecord};
use crate::report::StepEvidence;
use crate::scenario::{fork_expect, scenarios, traps, Expect, TrapCall};
use crate::stellar::{ErrorKind, Event, Invocation, Policy, Safe};

/// `b_rate` and `d_rate` are 12-decimal fixed point; every ratio in a reserve config is 7.
pub const RATE_DP: u32 = 12;
pub const RATIO_DP: u32 = 7;

// --- Verdicts -----------------------------------------------------------------------

/// How one step landed, most severe first. `Unexpected` is the calibration-bug
/// signature: the fork side did something its own scenario did not ask for, on both
/// sides at once, which is not protocol divergence and must not read as one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Verdict {
    NoCall,
    Match,
    Expected,
    Unexpected,
    Violation,
}

impl Verdict {
    pub fn glyph(self) -> char {
        match self {
            Verdict::NoCall => '·',
            Verdict::Match => '▪',
            Verdict::Expected => '◆',
            Verdict::Unexpected => '!',
            Verdict::Violation => '✖',
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Verdict::NoCall => "no call",
            Verdict::Match => "match",
            Verdict::Expected => "expected fork divergence",
            Verdict::Unexpected => "off-scenario outcome",
            Verdict::Violation => "violation",
        }
    }
}

/// One cell of the divergence strip.
#[derive(Clone, Debug)]
pub struct Cell {
    pub scenario: String,
    pub label: String,
    pub verdict: Verdict,
    /// Evidence label with no declared expectation (renamed or ad-hoc step).
    pub unknown: bool,
}

// --- Flow attribution ---------------------------------------------------------------

/// One ADR-0008 restriction-7 loss event, decoded to its amounts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Loss {
    /// `debt_setoff(user, asset, b_tokens_burned, d_tokens_repaid)`
    Setoff { b_burned: i128, d_repaid: i128 },
    /// `defaulted_debt(user, asset, d_tokens_burnt)`
    Defaulted { d_burnt: i128 },
    /// `collateral_orphaned(user, asset, b_tokens)`
    Orphaned { b_tokens: i128 },
    /// `orphan_settled(asset, b_tokens)`
    Settled { b_tokens: i128 },
}

impl Loss {
    pub fn name(&self) -> &'static str {
        match self {
            Loss::Setoff { .. } => "debt_setoff",
            Loss::Defaulted { .. } => "defaulted_debt",
            Loss::Orphaned { .. } => "collateral_orphaned",
            Loss::Settled { .. } => "orphan_settled",
        }
    }
}

/// An attribution line: the ADR rule, whether the numbers met it, and the residual.
#[derive(Clone, Debug)]
pub struct Note {
    pub ok: bool,
    pub text: Safe,
}

/// Per step, per reserve, per judged side: what moved and what said it could.
#[derive(Clone, Debug)]
pub struct Flow {
    /// The side this row was computed from; never relabel another side's failure.
    pub side: char,
    pub scenario: String,
    pub label: String,
    pub tok: Tok,
    pub d_b_supply: i128,
    pub d_d_supply: i128,
    pub d_custody: i128,
    pub d_b_rate: i128,
    pub losses: Vec<Loss>,
    pub notes: Vec<Note>,
}

impl Flow {
    /// Interest lifts `b_rate` on literally every step, so rate drift alone is not a
    /// move worth a row here; the RATES tab is where that belongs. A row means supply,
    /// debt or custody actually shifted.
    pub fn quiet(&self) -> bool {
        self.d_b_supply == 0 && self.d_d_supply == 0 && self.d_custody == 0
    }
    pub fn failed(&self) -> bool {
        self.notes.iter().any(|n| !n.ok)
    }
}

// --- Rates --------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Point {
    pub label: String,
    pub b_rate: i128,
    pub d_rate: i128,
    /// The step emitted `defaulted_debt` for this reserve, which is the only sanctioned
    /// reason for `b_rate` to fall.
    pub defaulted: bool,
    /// `b_supply` was zero, so the fork's declared no-denominator deviation applies and
    /// `b_rate` is preserved rather than written down.
    pub no_denominator: bool,
}

#[derive(Clone, Debug)]
pub struct Series {
    /// The side whose rates these are; under fork-vs-fork each side is judged.
    pub side: char,
    pub tok: Tok,
    pub points: Vec<Point>,
    pub notes: Vec<Note>,
}

// --- Traps --------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct TrapRow {
    pub call: &'static str,
    pub want: u32,
    pub a: Safe,
    pub b: Safe,
    pub ok: bool,
    pub runs: usize,
}

// --- Health -------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Gauge {
    pub tok: Tok,
    /// Utilisation and its ceiling, both 7dp.
    pub util: i128,
    pub max_util: i128,
    /// Supply and its cap, in the reserve's own token units.
    pub supply: i128,
    pub cap: i128,
    pub decimals: u32,
}

impl Gauge {
    /// Utilisation as a fraction of its ceiling, saturated at 1.0, for the bar.
    pub fn util_frac(&self) -> f64 {
        frac(self.util, self.max_util)
    }
    pub fn cap_frac(&self) -> f64 {
        if uncapped(self.cap) {
            0.0
        } else {
            frac(self.supply, self.cap)
        }
    }
    pub fn over_util(&self) -> bool {
        self.max_util > 0 && self.util > self.max_util
    }
    pub fn over_cap(&self) -> bool {
        self.cap > 0 && !uncapped(self.cap) && self.supply > self.cap
    }
}

fn frac(n: i128, d: i128) -> f64 {
    if d <= 0 {
        return 0.0;
    }
    ((n as f64) / (d as f64)).clamp(0.0, 1.0)
}

// --- Orphan custody -----------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Custody {
    Never,
    Held,
    Settled,
}

#[derive(Clone, Debug)]
pub struct Orphan {
    pub tok: Tok,
    pub state: Custody,
    pub amount: i128,
    pub since: Option<String>,
    pub settled_at: Option<String>,
    pub notes: Vec<Note>,
}

// --- The accumulator ----------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct Timeline {
    pub cells: Vec<Cell>,
    pub flows: Vec<Flow>,
    pub rates: Vec<Series>,
    pub traps: Vec<TrapRow>,
    pub orphans: Vec<Orphan>,
    /// Evidence labels no scenario declares; surfaced, never silently judged.
    pub unknown_labels: Vec<String>,
}

impl Timeline {
    /// `steps` must be in execution order; `App` sorts evidence into plan order before
    /// it gets here, because a delta against the wrong predecessor is worse than none.
    pub fn build(steps: &[StepEvidence], m: &PairManifest, policy: Policy) -> Self {
        let want = expectations();
        let cells = cells(steps, &want);
        let unknown_labels = cells
            .iter()
            .filter(|c| c.unknown)
            .map(|c| c.label.clone())
            .collect();
        // Operator snapshots remain visible in the strip, but cannot alter the
        // ordered scenario history that certifies flows, rates or custody.
        let judged: Vec<&StepEvidence> = steps
            .iter()
            .filter(|s| s.scenario != crate::run::OPERATOR_SCENARIO)
            .collect();
        Self {
            cells,
            unknown_labels,
            flows: flows(policy, &judged, m),
            rates: rates(policy, &judged, m),
            traps: trap_rows(policy, &judged),
            orphans: orphans(policy, &judged, m),
        }
    }

    pub fn violations(&self) -> usize {
        self.flows.iter().filter(|f| f.failed()).count()
            + self
                .rates
                .iter()
                .map(|s| s.notes.iter().filter(|n| !n.ok).count())
                .sum::<usize>()
            + self.traps.iter().filter(|t| t.runs > 0 && !t.ok).count()
            + self
                .orphans
                .iter()
                .map(|o| o.notes.iter().filter(|n| !n.ok).count())
                .sum::<usize>()
    }
}

/// Every scenario step's declared expectation, resolved for the fork side. `PerSide`
/// defers to `fork_expect`, which is what the trap rows and the strip both judge against.
fn expectations() -> BTreeMap<String, Expect> {
    let mut out = BTreeMap::new();
    let mut add = |label: &str, expect: Expect, step: &crate::scenario::Step| {
        let e = match expect {
            Expect::PerSide => fork_expect(label, step),
            other => other,
        };
        out.insert(label.to_string(), e);
    };
    for sc in scenarios() {
        for s in &sc.steps {
            add(s.label, s.expect, &s.step);
        }
    }
    for s in traps() {
        add(s.label, s.expect, &s.step);
    }
    out
}

fn meets(expect: Expect, inv: Option<&Invocation>) -> bool {
    let Some(i) = inv else { return true };
    match expect {
        Expect::Ok => i.ok,
        Expect::Err(c) => matches!(&i.error, Some(ErrorKind::Contract(x)) if *x == c),
        Expect::AnyErr => !i.ok,
        Expect::Any | Expect::PerSide => true,
    }
}

fn cells(steps: &[StepEvidence], want: &BTreeMap<String, Expect>) -> Vec<Cell> {
    steps
        .iter()
        .map(|s| {
            // An evidence label with no declared expectation cannot be judged
            // "match": flag it so renamed labels make noise instead of quietly
            // defaulting to `Any`. Operator steps are ad-hoc by construction and
            // are already excluded from the verdict everywhere else.
            let unknown =
                !want.contains_key(&s.label) && s.scenario != crate::run::OPERATOR_SCENARIO;
            let verdict = if s.diff.iter().any(|(_, c)| matches!(c, Class::Violation(_))) {
                Verdict::Violation
            } else if !meets(
                want.get(&s.label).copied().unwrap_or(Expect::Any),
                s.b.invocation.as_ref(),
            ) {
                Verdict::Unexpected
            } else if s.diff.iter().any(|(_, c)| matches!(c, Class::Expected(_))) {
                Verdict::Expected
            } else if s.b.invocation.is_none() {
                Verdict::NoCall
            } else {
                Verdict::Match
            };
            Cell {
                scenario: s.scenario.clone(),
                label: s.label.clone(),
                verdict,
                unknown,
            }
        })
        .collect()
}

// --- Reading a snapshot -------------------------------------------------------------

/// Strict reserve field read: `None` when the reserve, the group, the field
/// or the value itself is missing or unparseable. Verdicts must use this.
fn try_reserve_field(s: &Snapshot, tok: Tok, group: &str, field: &str) -> Option<i128> {
    s.reserves
        .get(&tok)?
        .get(group)?
        .get(field)
        .and_then(try_num)
}

/// Display-only leniency (gauges); a verdict must use [`try_reserve_field`]
/// and fail on an unobservable field instead of coercing to zero.
fn reserve_field(s: &Snapshot, tok: Tok, group: &str, field: &str) -> i128 {
    try_reserve_field(s, tok, group, field).unwrap_or(0)
}

/// The reserve's asset address, which the snapshot carries and the manifest does not.
fn asset_of(s: &Snapshot, tok: Tok) -> Option<String> {
    s.reserves
        .get(&tok)?
        .get("asset")?
        .as_str()
        .map(str::to_string)
}

fn index_of(m: &SideManifest, tok: Tok) -> Option<u32> {
    m.reserves.get(&tok).map(|r| r.index)
}

/// Amount a Soroban `Map<u32, i128>` holds at `key`, in either serialization.
/// `None` means the map itself was missing or malformed; an absent key inside
/// a genuine map is `Some(0)`, which is what absence means there.
fn at_index(v: &Value, key: u32) -> Option<i128> {
    let entries = strict_map(v)?;
    match entries
        .into_iter()
        .find(|(k, _)| k.parse::<u32>().ok() == Some(key))
    {
        Some((_, v)) => try_num(&v),
        None => Some(0),
    }
}

/// The pool-owned custody position's noncollateral supply for one reserve.
/// `None` = the custody row (or the reserve's index) could not be observed.
fn custody_supply(s: &Snapshot, m: &SideManifest, tok: Tok) -> Option<i128> {
    let i = index_of(m, tok)?;
    let v = s.positions.get("pool")?.get("supply")?;
    at_index(v, i)
}

/// The restriction-7 loss event symbols, as published by the contracts.
const LOSS_NAMES: [&str; 4] = [
    "debt_setoff",
    "defaulted_debt",
    "collateral_orphaned",
    "orphan_settled",
];

fn event_name(e: &Event) -> Option<&str> {
    e.topics.first()?.get("symbol")?.as_str()
}

/// Every user-attributed loss and stock bad debt place the asset after the
/// user; only `orphan_settled` carries the asset alone. A user address
/// matching an asset is not an asset witness.
fn loss_asset(e: &Event) -> Option<&str> {
    let index = match event_name(e)? {
        "debt_setoff" | "defaulted_debt" | "collateral_orphaned" | "bad_debt" => {
            e.topics.get(1)?.get("address")?.as_str()?;
            2
        }
        "orphan_settled" => 1,
        _ => return None,
    };
    if e.topics.len() != index + 1 {
        return None;
    }
    e.topics.get(index)?.get("address")?.as_str()
}

/// The one loss grammar: the contracts publish strictly positive quantities,
/// so a zero, negative, or malformed amount is invalid evidence — it explains
/// no accounting move and admits no custody allowance. Setoff carries
/// (b burned, d repaid); every other loss one quantity.
fn loss_amounts(e: &Event) -> Option<(i128, i128)> {
    match event_name(e)? {
        "debt_setoff" => {
            let values = e.value.get("vec")?.as_array()?;
            if values.len() != 2 {
                return None;
            }
            let b = values[0].get("i128").and_then(try_num)?;
            let d = values[1].get("i128").and_then(try_num)?;
            (b > 0 && d > 0).then_some((b, d))
        }
        "defaulted_debt" | "collateral_orphaned" | "orphan_settled" | "bad_debt" => {
            let amount = e.value.get("i128").and_then(try_num)?;
            (amount > 0).then_some((amount, 0))
        }
        _ => None,
    }
}

/// True when the record *claims* a fork loss by symbol, whatever the payload:
/// a claimed occurrence is judged, not silently dropped, and only the full
/// grammar above can authenticate it.
pub fn claims_pool_loss(rec: &StepRecord) -> bool {
    rec.invocation.iter().any(|i| {
        i.events.iter().any(|e| {
            matches!(
                event_name(e),
                Some("debt_setoff") | Some("defaulted_debt") | Some("collateral_orphaned")
            )
        })
    })
}

/// Only successful, internally consistent pool invocations can explain loss.
pub fn emits_named_loss(rec: &StepRecord, name: &str, side: &SideManifest) -> bool {
    let Some(inv) = rec
        .invocation
        .as_ref()
        .filter(|i| i.ok && i.error.is_none())
    else {
        return false;
    };
    inv.events.iter().any(|e| {
        event_name(e) == Some(name)
            && e.contract == side.contracts.pool
            && loss_asset(e).is_some_and(|asset| {
                side.reserves
                    .keys()
                    .any(|tok| side.contracts.token(*tok) == asset)
            })
            && loss_amounts(e).is_some()
    })
}

/// Genuine fork loss creation, not stock backstop routing or orphan settlement.
pub fn emits_pool_loss(rec: &StepRecord, side: &SideManifest) -> bool {
    ["debt_setoff", "defaulted_debt", "collateral_orphaned"]
        .iter()
        .any(|name| emits_named_loss(rec, name, side))
}

/// Decode genuine pool loss events for this asset. Invalid evidence cannot
/// explain an accounting move, even when another event in the call is valid.
fn losses_for(policy: Policy, events: &[Event], asset: &str, pool: &str) -> (Vec<Loss>, Vec<Safe>) {
    let mut losses = Vec::new();
    let mut problems = Vec::new();
    for e in events {
        let Some(name) = event_name(e).filter(|name| LOSS_NAMES.contains(name)) else {
            continue;
        };
        if e.contract != pool {
            problems.push(policy_finding(
                policy,
                format!("{name} event emitted by non-pool contract {}", e.contract),
            ));
            continue;
        }
        let Some(event_asset) = loss_asset(e) else {
            problems.push(policy_finding(
                policy,
                format!("{name} event has malformed asset topics"),
            ));
            continue;
        };
        if event_asset != asset {
            continue;
        }
        let Some((amount, debt)) = loss_amounts(e) else {
            problems.push(policy_finding(
                policy,
                format!(
                    "{name} event for {asset} carries a missing, nonpositive or malformed i128 amount"
                ),
            ));
            continue;
        };
        losses.push(match name {
            "debt_setoff" => Loss::Setoff {
                b_burned: amount,
                d_repaid: debt,
            },
            "defaulted_debt" => Loss::Defaulted { d_burnt: amount },
            "collateral_orphaned" => Loss::Orphaned { b_tokens: amount },
            "orphan_settled" => Loss::Settled { b_tokens: amount },
            _ => unreachable!("loss name checked above"),
        });
    }
    (losses, problems)
}

const MOVERS: [&str; 12] = [
    "supply",
    "supply_collateral",
    "withdraw",
    "withdraw_collateral",
    "borrow",
    "repay",
    "fill_auction",
    "new_auction",
    "bad_debt",
    "defaulted_debt",
    "gulp",
    "draw",
];

/// True when the step emitted an event that can move `b_supply`/`d_supply`.
/// The "unexplained supply move" check must listen for these, not for *any*
/// event: an unrelated emit must not silence the ADR 0008 §8 equation.
/// Names are the contracts' real published symbols (pool/src/events.rs,
/// backstop/src/events.rs), verified against recorded runs — an invented name
/// here false-flags ordinary steps as "unexplained".
fn supply_movers(events: &[Event]) -> bool {
    events
        .iter()
        .any(|e| event_name(e).is_some_and(|n| MOVERS.contains(&n)))
}

/// Which sides the temporal checks judge: the fork always; under fork-vs-fork
/// both sides are forks and both must reconcile.
fn temporal_sides(m: &PairManifest) -> &'static [char] {
    if m.profile == Profile::ForkVsFork {
        &['a', 'b']
    } else {
        &['b']
    }
}

fn flows(policy: Policy, steps: &[&StepEvidence], m: &PairManifest) -> Vec<Flow> {
    let mut out = Vec::new();
    for (i, cur) in steps.iter().enumerate() {
        for &side in temporal_sides(m) {
            let sm = if side == 'a' { &m.a } else { &m.b };
            let rec = if side == 'a' { &cur.a } else { &cur.b };
            // The step's own fresh baseline brackets the call exactly; evidence
            // written before `StepRecord::before` existed falls back to the
            // same side's previous after-snapshot.
            let Some(before) = rec.before.as_ref().or_else(|| {
                i.checked_sub(1).map(|j| {
                    &(if side == 'a' {
                        &steps[j].a
                    } else {
                        &steps[j].b
                    })
                    .snapshot
                })
            }) else {
                continue;
            };
            let pool = sm.contracts.pool.clone();
            for tok in Tok::RESERVES {
                let here = rec.snapshot.reserves.contains_key(&tok);
                let was = before.reserves.contains_key(&tok);
                if !here && !was {
                    continue; // reserve not configured on this side
                }
                let mut flow = Flow {
                    side,
                    scenario: cur.scenario.clone(),
                    label: cur.label.clone(),
                    tok,
                    d_b_supply: 0,
                    d_d_supply: 0,
                    d_custody: 0,
                    d_b_rate: 0,
                    losses: Vec::new(),
                    notes: Vec::new(),
                };
                let mut broken: Vec<Safe> = Vec::new();
                if !here || !was {
                    broken.push(policy_finding(
                        policy,
                        format!(
                            "{side}: {}: reserve not observable on one side of {}",
                            tok.as_str(),
                            cur.label
                        ),
                    ));
                }
                let events: &[Event] = rec
                    .invocation
                    .as_ref()
                    .filter(|i| i.ok && i.error.is_none())
                    .map(|i| i.events.as_slice())
                    .unwrap_or(&[]);
                let (losses, mut problems) =
                    match asset_of(&rec.snapshot, tok).or_else(|| asset_of(before, tok)) {
                        Some(asset) => losses_for(policy, events, &asset, &pool),
                        None => (
                            Vec::new(),
                            vec![policy_finding(
                                policy,
                                format!("{side}: {}: reserve asset not observable", tok.as_str()),
                            )],
                        ),
                    };
                broken.append(&mut problems);
                let fields = |s: &Snapshot| {
                    (
                        try_reserve_field(s, tok, "data", "b_supply"),
                        try_reserve_field(s, tok, "data", "d_supply"),
                        try_reserve_field(s, tok, "data", "b_rate"),
                    )
                };
                let (b0, d0, r0) = fields(before);
                let (b1, d1, r1) = fields(&rec.snapshot);
                match (b0, d0, r0, b1, d1, r1) {
                    (Some(b0), Some(d0), Some(r0), Some(b1), Some(d1), Some(r1)) => {
                        match (b1.checked_sub(b0), d1.checked_sub(d0), r1.checked_sub(r0)) {
                            (Some(db), Some(dd), Some(dr)) => {
                                flow.d_b_supply = db;
                                flow.d_d_supply = dd;
                                flow.d_b_rate = dr;
                                match (
                                    custody_supply(before, sm, tok),
                                    custody_supply(&rec.snapshot, sm, tok),
                                ) {
                                    (Some(c0), Some(c1)) => match c1.checked_sub(c0) {
                                        Some(d) => flow.d_custody = d,
                                        None => broken.push(policy_finding(
                                            policy,
                                            format!(
                                                "{side}: {}: custody delta overflows i128",
                                                tok.as_str()
                                            ),
                                        )),
                                    },
                                    (None, None) => {} // not in the manifest: no custody claim
                                    _ => broken.push(policy_finding(
                                        policy,
                                        format!(
                                            "{side}: {}: custody position not observable",
                                            tok.as_str()
                                        ),
                                    )),
                                }
                                flow.losses = losses;
                                flow.notes = attribute(
                                    policy,
                                    &flow.losses,
                                    flow.d_b_supply,
                                    flow.d_d_supply,
                                    flow.d_custody,
                                    flow.d_b_rate,
                                    b0,
                                    !flow.losses.is_empty() || supply_movers(events),
                                );
                            }
                            _ => broken.push(policy_finding(
                                policy,
                                format!(
                                    "{side}: {}: reserve supply deltas overflow i128 at {}",
                                    tok.as_str(),
                                    cur.label
                                ),
                            )),
                        }
                    }
                    _ => broken.push(policy_finding(
                        policy,
                        format!(
                            "{side}: {}: reserve data not observable across {}",
                            tok.as_str(),
                            cur.label
                        ),
                    )),
                }
                flow.notes
                    .extend(broken.into_iter().map(|text| Note { ok: false, text }));
                if !flow.quiet() || !flow.losses.is_empty() || flow.failed() {
                    out.push(flow);
                }
            }
        }
    }
    out
}

/// The ADR-0008 restriction-7 and restriction-4 equations, composed across all
/// loss events the step emitted. One bad-debt/final-fill call may lawfully
/// `debt_setoff` same-reserve supply, default the rest and orphan collateral in
/// the same transaction, so each event kind's rule is judged against the total
/// its kind explains, never against the raw whole-call delta in isolation
/// (that composition was a false-positive machine). A user-liquidation fill
/// transfers positions without changing global supplies; other unsupported
/// residuals remain failures, never an ordinary-event waiver.
// Rethreading these ADR-0008 §8 inputs through a grouped struct would move
// arithmetic that composes loss-attribution notes byte-for-byte into report
// evidence. Keep the flat signature to preserve pair analysis output.
#[allow(clippy::too_many_arguments)]
fn attribute(
    policy: Policy,
    losses: &[Loss],
    d_b_supply: i128,
    d_d_supply: i128,
    d_custody: i128,
    d_b_rate: i128,
    b_supply_before: i128,
    had_events: bool,
) -> Vec<Note> {
    let mut n = Vec::new();
    let mut note = |ok: bool, text: Safe| n.push(Note { ok, text });
    if losses.is_empty() {
        // ADR 0008 §8: a supply change no event explains is an unexplained accounting change.
        if !had_events && (d_b_supply != 0 || d_d_supply != 0) {
            note(
                false,
                policy_finding(
                    policy,
                    format!(
                        "unexplained: Δb_supply {d_b_supply}, Δd_supply {d_d_supply} with no event emitted"
                    ),
                ),
            );
        }
        return n;
    }
    let mut setoff_b = 0i128;
    let mut setoff_d = 0i128;
    let mut defaulted_d = 0i128;
    let mut orphaned_b = 0i128;
    let mut settled_b = 0i128;
    let mut overflow = false;
    let add = |acc: &mut i128, v: i128, overflow: &mut bool| match acc.checked_add(v) {
        Some(x) => *acc = x,
        None => *overflow = true,
    };
    for l in losses {
        match l {
            Loss::Setoff { b_burned, d_repaid } => {
                add(&mut setoff_b, *b_burned, &mut overflow);
                add(&mut setoff_d, *d_repaid, &mut overflow);
            }
            Loss::Defaulted { d_burnt } => add(&mut defaulted_d, *d_burnt, &mut overflow),
            Loss::Orphaned { b_tokens } => add(&mut orphaned_b, *b_tokens, &mut overflow),
            Loss::Settled { b_tokens } => add(&mut settled_b, *b_tokens, &mut overflow),
        }
    }
    if overflow {
        note(false, Safe::literal("loss event totals overflow i128"));
        return n;
    }
    let has_default = losses.iter().any(|l| matches!(l, Loss::Defaulted { .. }));
    let has_orphan = losses.iter().any(|l| matches!(l, Loss::Orphaned { .. }));
    let has_settle = losses.iter().any(|l| matches!(l, Loss::Settled { .. }));

    if has_default {
        // r7: the write-down moves b_rate, never b_supply — beyond what a
        // same-call debt_setoff burn already explains.
        let explained = setoff_b.checked_neg().is_some_and(|e| d_b_supply == e);
        note(
            explained,
            policy_finding(
                policy,
                format!(
                    "defaulted_debt {defaulted_d} d: b_supply must not move beyond same-call setoff burns ({setoff_b} b), Δ {d_b_supply}"
                ),
            ),
        );
        if b_supply_before > 0 {
            note(
                d_b_rate < 0,
                policy_finding(
                    policy,
                    format!("b_supply {b_supply_before} > 0 so b_rate must fall, Δ {d_b_rate}"),
                ),
            );
        } else {
            note(
                d_b_rate == 0,
                policy_finding(
                    policy,
                    format!(
                        "b_supply 0: declared no-denominator deviation preserves b_rate, Δ {d_b_rate}"
                    ),
                ),
            );
        }
    }
    if has_orphan || has_settle {
        // r7/r4: custody moves only by orphans arriving and settlements leaving.
        let want = orphaned_b.checked_sub(settled_b);
        note(
            want == Some(d_custody),
            policy_finding(
                policy,
                format!(
                    "custody must move by {} (orphaned {orphaned_b}, settled {settled_b}), Δ {d_custody}",
                    want.map(|w| w.to_string())
                        .unwrap_or_else(|| "overflow".into())
                ),
            ),
        );
    }
    // ADR 0008 §8: all observed loss-call supply changes must reconcile.
    let rb = d_b_supply
        .checked_add(setoff_b)
        .and_then(|x| x.checked_add(settled_b));
    let rd = d_d_supply
        .checked_add(setoff_d)
        .and_then(|x| x.checked_add(defaulted_d));
    match (rb, rd) {
        (Some(rb), Some(rd)) => note(
            rb == 0 && rd == 0,
            policy_finding(
                policy,
                format!(
                    "loss attribution must explain the move: Δb_supply residual {rb}, Δd_supply residual {rd}"
                ),
            ),
        ),
        _ => note(false, Safe::literal("loss attribution overflows i128")),
    }
    n
}

fn rates(policy: Policy, steps: &[&StepEvidence], m: &PairManifest) -> Vec<Series> {
    let mut out = Vec::new();
    for &side in temporal_sides(m) {
        let sm = if side == 'a' { &m.a } else { &m.b };
        let pool = sm.contracts.pool.clone();
        let rec: fn(&StepEvidence) -> &StepRecord = if side == 'a' { |s| &s.a } else { |s| &s.b };
        for tok in Tok::RESERVES {
            let asset = steps
                .iter()
                .find_map(|s| asset_of(&rec(s).snapshot, tok))
                .unwrap_or_default();
            let mut points: Vec<Point> = Vec::new();
            let mut notes = Vec::new();
            for s in steps {
                let r = rec(s);
                if !r.snapshot.reserves.contains_key(&tok) {
                    continue; // reserve not configured on this side
                }
                let (b_rate, d_rate) = match (
                    try_reserve_field(&r.snapshot, tok, "data", "b_rate"),
                    try_reserve_field(&r.snapshot, tok, "data", "d_rate"),
                ) {
                    (Some(b), Some(d)) => (b, d),
                    _ => {
                        notes.push(Note {
                            ok: false,
                            text: policy_finding(
                                policy,
                                format!(
                                    "{side}: {}: rate fields not observable at {}",
                                    tok.as_str(),
                                    s.label
                                ),
                            ),
                        });
                        continue;
                    }
                };
                let (losses, problems) =
                    match r.invocation.as_ref().filter(|i| i.ok && i.error.is_none()) {
                        Some(i) => losses_for(policy, &i.events, &asset, &pool),
                        None => (Vec::new(), Vec::new()),
                    };
                for p in problems {
                    notes.push(Note {
                        ok: false,
                        text: policy_finding(policy, format!("{side}: {}: {p}", tok.as_str())),
                    });
                }
                // A forged or malformed default event must not excuse a fall.
                let defaulted = losses.iter().any(|l| matches!(l, Loss::Defaulted { .. }));
                if let Some(prev) = points.last() {
                    // r7: interest only lifts b_rate; the sanctioned fall is a default.
                    if b_rate < prev.b_rate && !defaulted {
                        notes.push(Note {
                            ok: false,
                            text: policy_finding(
                                policy,
                                format!(
                                    "{side}: {}: b_rate fell {} -> {} at {} with no defaulted_debt",
                                    tok.as_str(),
                                    prev.b_rate,
                                    b_rate,
                                    s.label
                                ),
                            ),
                        });
                    }
                }
                points.push(Point {
                    label: s.label.clone(),
                    b_rate,
                    d_rate,
                    defaulted,
                    no_denominator: defaulted
                        && try_reserve_field(&r.snapshot, tok, "data", "b_supply") == Some(0),
                });
            }
            out.push(Series {
                side,
                tok,
                points,
                notes,
            });
        }
    }
    out
}

pub fn outcome(policy: Policy, inv: Option<&Invocation>) -> Safe {
    match inv {
        None => Safe::literal("-"),
        Some(i) if i.ok => Safe::literal("ok"),
        Some(i) => i
            .error
            .as_ref()
            .map(|k| Safe::error_summary(policy, k))
            .unwrap_or_else(|| Safe::literal("failed")),
    }
}

fn trap_rows(policy: Policy, steps: &[&StepEvidence]) -> Vec<TrapRow> {
    TrapCall::ALL
        .iter()
        .map(|t| {
            let hits: Vec<&StepEvidence> = steps
                .iter()
                .copied()
                .filter(|s| s.label == t.label())
                .collect();
            let want = t.fork_error();
            let ok = !hits.is_empty()
                && hits.iter().all(|s| {
                    matches!(
                        s.b.invocation.as_ref().and_then(|i| i.error.as_ref()),
                        Some(ErrorKind::Contract(c)) if *c == want
                    )
                });
            // Every run of a trap must land the same way; show the disagreement if not.
            let side = |f: &dyn Fn(&StepEvidence) -> Safe| -> Safe {
                let mut seen: Vec<String> = hits.iter().map(|s| f(s).to_string()).collect();
                seen.dedup();
                seen.sort();
                seen.dedup();
                if seen.is_empty() {
                    Safe::literal("not run")
                } else {
                    Safe::error_chain(policy, &anyhow::anyhow!("{}", seen.join(" / ")))
                }
            };
            TrapRow {
                call: t.label(),
                want,
                a: side(&|s| outcome(policy, s.a.invocation.as_ref())),
                b: side(&|s| outcome(policy, s.b.invocation.as_ref())),
                ok,
                runs: hits.len(),
            }
        })
        .collect()
}

fn orphans(policy: Policy, steps: &[&StepEvidence], m: &PairManifest) -> Vec<Orphan> {
    Tok::RESERVES
        .iter()
        .map(|&tok| {
            let mut state = Custody::Never;
            let mut amount = 0i128;
            let mut since = None;
            let mut settled_at = None;
            let mut notes = Vec::new();
            let Some(i) = index_of(&m.b, tok) else {
                // Not in the manifest: there is no custody claim to make for it.
                return Orphan {
                    tok,
                    state,
                    amount,
                    since,
                    settled_at,
                    notes,
                };
            };
            for s in steps {
                let Some(held) =
                    s.b.snapshot
                        .positions
                        .get("pool")
                        .and_then(|p| p.get("supply"))
                        .and_then(|v| at_index(v, i))
                else {
                    notes.push(Note {
                        ok: false,
                        text: policy_finding(
                            policy,
                            format!(
                                "custody supply not observable for {} at {}",
                                tok.as_str(),
                                s.label
                            ),
                        ),
                    });
                    continue;
                };
                if held > 0 && state != Custody::Held {
                    state = Custody::Held;
                    since = Some(s.label.clone());
                } else if held == 0 && state == Custody::Held {
                    state = Custody::Settled;
                    settled_at = Some(s.label.clone());
                    // r4: the orphan only leaves once the reserve carries no debt.
                    match try_reserve_field(&s.b.snapshot, tok, "data", "d_supply") {
                        Some(d) => notes.push(Note {
                            ok: d == 0,
                            text: policy_finding(
                                policy,
                                format!("settled with d_supply {d}, which must be 0"),
                            ),
                        }),
                        None => notes.push(Note {
                            ok: false,
                            text: policy_finding(
                                policy,
                                format!("d_supply not observable at {}", s.label),
                            ),
                        }),
                    }
                }
                amount = held.max(amount);
                // Custody must never hold collateral or liabilities, only supply.
                if let Some(p) = s.b.snapshot.positions.get("pool") {
                    for kind in ["collateral", "liabilities"] {
                        match p.get(kind).map(|x| at_index(x, i)) {
                            Some(Some(v)) if v != 0 => notes.push(Note {
                                ok: false,
                                text: policy_finding(
                                    policy,
                                    format!("custody holds {kind} {v} at {}", s.label),
                                ),
                            }),
                            Some(None) => notes.push(Note {
                                ok: false,
                                text: policy_finding(
                                    policy,
                                    format!("custody {kind} map not observable at {}", s.label),
                                ),
                            }),
                            _ => {}
                        }
                    }
                }
            }
            Orphan {
                tok,
                state,
                amount,
                since,
                settled_at,
                notes,
            }
        })
        .collect()
}

/// Utilisation and cap headroom, read straight off one snapshot so the gauges track
/// whichever step the operator has selected.
pub fn gauges(s: &Snapshot, m: &PairManifest) -> Vec<Gauge> {
    Tok::RESERVES
        .iter()
        .map(|&tok| {
            let b_supply = reserve_field(s, tok, "data", "b_supply");
            let b_rate = reserve_field(s, tok, "data", "b_rate");
            let d_supply = reserve_field(s, tok, "data", "d_supply");
            let d_rate = reserve_field(s, tok, "data", "d_rate");
            let scale = 10i128.pow(RATIO_DP);
            // Display-only gauges: saturate instead of panicking on absurd
            // observed magnitudes; a verdict never reads these.
            let supply = b_supply.saturating_mul(b_rate) / 10i128.pow(RATE_DP);
            let liabilities = d_supply.saturating_mul(d_rate) / 10i128.pow(RATE_DP);
            let util = if supply > 0 {
                liabilities
                    .saturating_mul(scale)
                    .checked_div(supply)
                    .unwrap_or(0)
            } else {
                0
            };
            Gauge {
                tok,
                util,
                max_util: reserve_field(s, tok, "config", "max_util"),
                supply,
                cap: reserve_field(s, tok, "config", "supply_cap"),
                decimals: m
                    .b
                    .reserves
                    .get(&tok)
                    .map(|r| r.decimals)
                    .unwrap_or_else(|| tok.decimals()),
            }
        })
        .collect()
}

/// Eight-level block sparkline over the series range. A flat series renders flat rather
/// than as noise, which is what makes a genuine `b_rate` step visible.
pub fn spark(vals: &[i128]) -> String {
    const BLOCKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let (Some(&lo), Some(&hi)) = (vals.iter().min(), vals.iter().max()) else {
        return String::new();
    };
    let span = hi.saturating_sub(lo);
    vals.iter()
        .map(|v| {
            if span == 0 {
                BLOCKS[0]
            } else {
                // u128 intermediate: (v - lo) * 7 can exceed i128 at range edges.
                let d = v.saturating_sub(lo) as u128;
                let i = ((d * 7) / span as u128) as usize;
                BLOCKS[i.min(7)]
            }
        })
        .collect()
}

/// A 7-decimal ratio as a percentage with two decimals, which is all the precision a
/// headroom reading can act on.
pub fn pct(v: i128) -> String {
    let h = v / 1_000;
    format!("{}.{:02}%", h / 100, (h % 100).abs())
}

/// A supply cap set to the type's maximum is not a cap; say so rather than printing
/// thirty digits of noise.
pub fn uncapped(cap: i128) -> bool {
    cap >= i128::MAX / 2
}

/// A proportional bar, for the headroom gauges.
pub fn bar(frac: f64, width: usize) -> String {
    let filled = ((frac.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::StepRecord;
    use crate::stellar::Invocation;
    use serde_json::json;

    use crate::stellar::Policy;

    const ASSET: &str = "CASSET";

    fn manifest() -> PairManifest {
        // The loss decoder only trusts events from the pool contract; the
        // shared TUI fixture's contract ids are empty.
        let mut m = crate::tui::app::tests::manifest();
        m.a.contracts.pool = "CPOOL".into();
        m.b.contracts.pool = "CPOOL".into();
        m
    }

    /// Every user-attributed loss and stock bad debt carry the user before the asset.
    fn ev(name: &str, value: Value) -> Event {
        let mut topics = vec![json!({"symbol": name})];
        if matches!(
            name,
            "debt_setoff" | "defaulted_debt" | "collateral_orphaned" | "bad_debt"
        ) {
            topics.push(json!({"address": "GUSER"}));
        }
        topics.push(json!({"address": ASSET}));
        Event {
            contract: "CPOOL".into(),
            topics,
            value,
        }
    }

    fn i128v(v: i128) -> Value {
        json!({"i128": v.to_string()})
    }

    /// Fixture-only: Invocations built by deserializing the exact test JSON
    /// bytes (the single sanctioned route into `Safe`/`RawText`/`ErrorKind`).
    fn fixture_inv(
        ok: bool,
        value: Option<Value>,
        error: Option<ErrorKind>,
        events: Vec<Event>,
        stderr: &str,
    ) -> Invocation {
        serde_json::from_value(json!({
            "ok": ok,
            "value": value,
            "error": error,
            "events": events,
            "stderr": stderr,
        }))
        .unwrap()
    }

    /// One step, described only by the numbers the timeline reads.
    fn step(
        label: &str,
        b_supply: i128,
        d_supply: i128,
        b_rate: i128,
        custody: i128,
        events: Vec<Event>,
    ) -> StepEvidence {
        let snapshot = crate::observe::Snapshot {
            positions: [
                (
                    "pool".to_string(),
                    json!({"collateral": {}, "liabilities": {}, "supply": {"0": custody.to_string()}}),
                ),
                (
                    "frodo".to_string(),
                    json!({"collateral": {}, "liabilities": {}, "supply": {}}),
                ),
            ]
            .into(),
            reserves: [(
                Tok::Stable,
                json!({
                    "asset": ASSET,
                    "config": {"max_util": 9_500_000, "supply_cap": "1000000"},
                    "data": {
                        "b_supply": b_supply.to_string(),
                        "d_supply": d_supply.to_string(),
                        "b_rate": b_rate.to_string(),
                        "d_rate": "1000000000000",
                    }
                }),
            )]
            .into(),
            config: json!({"bstop_rate": 0}),
            backstop_pool: json!({}),
            backstop_users: BTreeMap::new(),
            reward_zone: json!([]),
            balances: BTreeMap::new(),
            auctions: BTreeMap::new(),
            raw: BTreeMap::new(),
        };
        let rec = |inv: Option<Invocation>| StepRecord {
            label: label.into(),
            side: "s".into(),
            invocation: inv,
            before: None,
            snapshot: snapshot.clone(),
            violations: vec![],
        };
        StepEvidence {
            scenario: "t".into(),
            index: 0,
            label: label.into(),
            a: rec(None),
            b: rec(Some(fixture_inv(true, None, None, events, ""))),
            diff: vec![],
            findings: vec![],
        }
    }

    fn only_flow(steps: &[StepEvidence]) -> Flow {
        let t = Timeline::build(steps, &manifest(), Policy::Pair);
        t.flows.into_iter().next().expect("one flow")
    }

    fn failures(f: &Flow) -> Vec<String> {
        f.notes
            .iter()
            .filter(|n| !n.ok)
            .map(|n| n.text.to_string())
            .collect()
    }

    // --- ADR 0008 restriction 7 -----------------------------------------------------

    #[test]
    fn defaulted_debt_writes_down_the_rate_and_never_the_supply() {
        let good = only_flow(&[
            step("a", 1000, 500, 1_000_000_000_000, 0, vec![]),
            step(
                "b",
                1000,
                400,
                999_000_000_000,
                0,
                vec![ev("defaulted_debt", i128v(100))],
            ),
        ]);
        assert!(failures(&good).is_empty(), "{:?}", failures(&good));

        // b_supply moving inside a default is the unexplained accounting change the ADR
        // says closes routes.
        let bad = only_flow(&[
            step("a", 1000, 500, 1_000_000_000_000, 0, vec![]),
            step(
                "b",
                900,
                400,
                999_000_000_000,
                0,
                vec![ev("defaulted_debt", i128v(100))],
            ),
        ]);
        assert!(failures(&bad)
            .iter()
            .any(|t| t.contains("b_supply must not move")));
    }

    #[test]
    fn a_default_that_does_not_move_the_rate_is_caught() {
        let bad = only_flow(&[
            step("a", 1000, 500, 1_000_000_000_000, 0, vec![]),
            step(
                "b",
                1000,
                400,
                1_000_000_000_000,
                0,
                vec![ev("defaulted_debt", i128v(100))],
            ),
        ]);
        assert!(failures(&bad)
            .iter()
            .any(|t| t.contains("b_rate must fall")));
    }

    #[test]
    fn the_no_denominator_deviation_is_judged_as_declared() {
        // b_supply was already zero, so the fork preserves b_rate by design.
        let ok = only_flow(&[
            step("a", 0, 500, 1_000_000_000_000, 0, vec![]),
            step(
                "b",
                0,
                400,
                1_000_000_000_000,
                0,
                vec![ev("defaulted_debt", i128v(100))],
            ),
        ]);
        assert!(failures(&ok).is_empty(), "{:?}", failures(&ok));

        let moved = only_flow(&[
            step("a", 0, 500, 1_000_000_000_000, 0, vec![]),
            step(
                "b",
                0,
                400,
                999_000_000_000,
                0,
                vec![ev("defaulted_debt", i128v(100))],
            ),
        ]);
        assert!(failures(&moved)
            .iter()
            .any(|t| t.contains("no-denominator deviation")));
    }

    #[test]
    fn orphaned_collateral_moves_to_custody_leaving_b_supply_alone() {
        let ok = only_flow(&[
            step("a", 1000, 0, 1_000_000_000_000, 0, vec![]),
            step(
                "b",
                1000,
                0,
                1_000_000_000_000,
                70,
                vec![ev("collateral_orphaned", i128v(70))],
            ),
        ]);
        assert!(failures(&ok).is_empty(), "{:?}", failures(&ok));

        // Custody taking a different amount than the event announced is the whole point
        // of checking: the position and the event must agree.
        let mismatch = only_flow(&[
            step("a", 1000, 0, 1_000_000_000_000, 0, vec![]),
            step(
                "b",
                1000,
                0,
                1_000_000_000_000,
                60,
                vec![ev("collateral_orphaned", i128v(70))],
            ),
        ]);
        assert!(failures(&mismatch)
            .iter()
            .any(|t| t.contains("custody must move by 70")));
    }

    #[test]
    fn settling_the_orphan_burns_exactly_what_custody_held() {
        let ok = only_flow(&[
            step("a", 1000, 0, 1_000_000_000_000, 70, vec![]),
            step(
                "b",
                930,
                0,
                1_000_000_000_000,
                0,
                vec![ev("orphan_settled", i128v(70))],
            ),
        ]);
        assert!(failures(&ok).is_empty(), "{:?}", failures(&ok));

        let short = only_flow(&[
            step("a", 1000, 0, 1_000_000_000_000, 70, vec![]),
            step(
                "b",
                1000,
                0,
                1_000_000_000_000,
                0,
                vec![ev("orphan_settled", i128v(70))],
            ),
        ]);
        assert!(failures(&short)
            .iter()
            .any(|t| t.contains("Δb_supply residual 70")));
    }

    #[test]
    fn setoff_reports_its_residual_rather_than_guessing() {
        let f = only_flow(&[
            step("a", 1000, 500, 1_000_000_000_000, 0, vec![]),
            step(
                "b",
                960,
                460,
                1_000_000_000_000,
                0,
                vec![ev("debt_setoff", json!({"vec": [i128v(40), i128v(40)]}))],
            ),
        ]);
        let text = f
            .notes
            .iter()
            .map(|n| n.text.to_string())
            .collect::<Vec<_>>()
            .join("");
        assert!(text.contains("Δb_supply residual 0"), "{text}");
        assert!(text.contains("Δd_supply residual 0"), "{text}");
    }

    // --- ADR 0008 section 8 ---------------------------------------------------------

    #[test]
    fn a_supply_move_no_event_explains_is_flagged() {
        let f = only_flow(&[
            step("a", 1000, 0, 1_000_000_000_000, 0, vec![]),
            step("b", 800, 0, 1_000_000_000_000, 0, vec![]),
        ]);
        assert!(failures(&f).iter().any(|t| t.contains("unexplained")));
    }

    /// An event that cannot move supply must not silence the §8 equation: before
    /// the supply-mover gate, *any* emitted event muted the unexplained check.
    #[test]
    fn an_unrelated_event_does_not_silence_the_unexplained_supply_check() {
        let ev = |names: &[&str]| -> Vec<Event> {
            names
                .iter()
                .map(|n| Event {
                    contract: "C".into(),
                    topics: vec![json!({"symbol": n})],
                    value: json!(null),
                })
                .collect()
        };
        let f = only_flow(&[
            step("a", 1000, 0, 1_000_000_000_000, 0, vec![]),
            step("b", 1500, 0, 1_000_000_000_000, 0, ev(&["mint"])),
        ]);
        assert!(f.notes.iter().any(|n| !n.ok), "{:?}", f.notes);
    }

    /// Ordinary collateral supplies and auction fills move b_supply and emit
    /// their real symbols; invented names must not stand in for them.
    #[test]
    fn only_the_published_symbols_count_as_supply_movers() {
        let e = |n: &str| ev(n, json!(1));
        for n in [
            "supply_collateral",
            "withdraw_collateral",
            "fill_auction",
            "gulp",
            "draw",
        ] {
            assert!(supply_movers(&[e(n)]), "{n} is a real symbol");
        }
        for n in ["liquidate", "fill_user_liq", "auction_fill", "admin_funds"] {
            assert!(
                !supply_movers(&[e(n)]),
                "{n} is never emitted by the contracts"
            );
        }
    }

    #[test]
    fn operator_steps_are_not_flagged_as_unknown_labels() {
        let mut want = BTreeMap::new();
        want.insert("declared".to_string(), crate::scenario::Expect::Any);
        let mut op = step("operator_001_snapshot", 0, 0, 1_000_000_000_000, 0, vec![]);
        op.scenario = crate::run::OPERATOR_SCENARIO.into();
        let cs = cells(
            &[step("declared", 0, 0, 1_000_000_000_000, 0, vec![]), op],
            &want,
        );
        assert!(!cs[0].unknown, "declared labels are known");
        assert!(
            !cs[1].unknown,
            "ad-hoc operator steps are excluded from the check"
        );
        assert!(
            cells(
                &[step("renamed_step", 0, 0, 1_000_000_000_000, 0, vec![])],
                &want
            )[0]
            .unknown,
            "a renamed scenario label must still make noise"
        );
    }

    #[test]
    fn interest_drift_alone_does_not_make_a_flow_row() {
        // b_rate lifts on every single step; if that created a row the view would be
        // three rows of noise per step and unreadable.
        let t = Timeline::build(
            &[
                step("a", 1000, 0, 1_000_000_000_000, 0, vec![]),
                step("b", 1000, 0, 1_000_000_000_001, 0, vec![]),
            ],
            &manifest(),
            Policy::Pair,
        );
        assert!(t.flows.is_empty());
    }

    // --- rates ----------------------------------------------------------------------

    #[test]
    fn a_rate_fall_without_a_default_is_reported() {
        let t = Timeline::build(
            &[
                step("a", 1000, 0, 1_000_000_000_000, 0, vec![]),
                step("b", 1000, 0, 999_000_000_000, 0, vec![]),
            ],
            &manifest(),
            Policy::Pair,
        );
        let s = t
            .rates
            .iter()
            .find(|s| s.tok == Tok::Stable)
            .expect("stable series");
        assert_eq!(s.points.len(), 2);
        assert!(s
            .notes
            .iter()
            .any(|n| !n.ok && n.text.to_string().contains("b_rate fell")));
    }

    #[test]
    fn a_rate_fall_with_a_default_is_accepted() {
        let t = Timeline::build(
            &[
                step("a", 1000, 500, 1_000_000_000_000, 0, vec![]),
                step(
                    "b",
                    1000,
                    400,
                    999_000_000_000,
                    0,
                    vec![ev("defaulted_debt", i128v(100))],
                ),
            ],
            &manifest(),
            Policy::Pair,
        );
        let s = &t.rates[0];
        assert!(s.notes.is_empty());
        assert!(s.points[1].defaulted);
    }

    #[test]
    fn spark_is_flat_for_a_flat_series_and_climbs_for_a_rising_one() {
        assert_eq!(spark(&[5, 5, 5]), "▁▁▁");
        assert_eq!(spark(&[0, 7]), "▁█");
        assert_eq!(spark(&[]), "");
    }

    // --- traps ----------------------------------------------------------------------

    fn trap_step(label: &'static str, code: Option<u32>) -> StepEvidence {
        let mut s = step(label, 0, 0, 1, 0, vec![]);
        s.label = label.into();
        s.b.label = label.into();
        s.b.invocation = Some(fixture_inv(
            code.is_none(),
            None,
            code.map(ErrorKind::Contract),
            vec![],
            &code
                .map(|c| format!("error: Error(Contract, #{c})"))
                .unwrap_or_default(),
        ));
        s
    }

    #[test]
    fn a_trap_is_green_only_when_the_fork_returns_the_code_adr_0011_requires() {
        let m = manifest();
        let right = Timeline::build(
            &[trap_step("trap_backstop_claim", Some(1000))],
            &m,
            Policy::Pair,
        );
        let row = right
            .traps
            .iter()
            .find(|r| r.call == "trap_backstop_claim")
            .expect("row");
        assert!(row.ok);
        assert_eq!(row.want, 1000);

        // The wrong rejection is still a rejection; ADR 0011 names the code.
        let wrong = Timeline::build(
            &[trap_step("trap_backstop_claim", Some(1205))],
            &m,
            Policy::Pair,
        );
        assert!(
            !wrong
                .traps
                .iter()
                .find(|r| r.call == "trap_backstop_claim")
                .unwrap()
                .ok
        );

        // A trap that succeeded is the failure that matters most.
        let open = Timeline::build(&[trap_step("trap_backstop_claim", None)], &m, Policy::Pair);
        assert!(
            !open
                .traps
                .iter()
                .find(|r| r.call == "trap_backstop_claim")
                .unwrap()
                .ok
        );
    }

    #[test]
    fn a_trap_never_run_is_not_reported_as_passing() {
        let t = Timeline::build(&[], &manifest(), Policy::Pair);
        assert_eq!(t.traps.len(), TrapCall::ALL.len());
        assert!(t.traps.iter().all(|r| !r.ok && r.runs == 0));
        assert!(t.traps.iter().all(|r| r.b == Safe::literal("not run")));
    }

    // --- the strip ------------------------------------------------------------------

    #[test]
    fn an_off_scenario_outcome_does_not_read_as_protocol_divergence() {
        // `liq_frodo_collateral` is declared Ok. Failing it on the fork side is a
        // scenario-calibration bug, not a divergence, and the strip must say so.
        let mut s = step("liq_frodo_collateral", 0, 0, 1, 0, vec![]);
        s.label = "liq_frodo_collateral".into();
        s.b.invocation = Some(fixture_inv(
            false,
            None,
            Some(ErrorKind::Contract(1207)),
            vec![],
            "error: Error(Contract, #1207)",
        ));
        let t = Timeline::build(&[s], &manifest(), Policy::Pair);
        assert_eq!(t.cells[0].verdict, Verdict::Unexpected);
        assert_eq!(t.cells[0].verdict.glyph(), '!');
    }

    #[test]
    fn a_violation_outranks_everything_else_on_the_strip() {
        let mut s = step("x", 0, 0, 1, 0, vec![]);
        s.diff = vec![
            ("p".into(), Class::Expected("e".into())),
            ("q".into(), Class::Violation("v".into())),
        ];
        let t = Timeline::build(&[s], &manifest(), Policy::Pair);
        assert_eq!(t.cells[0].verdict, Verdict::Violation);
    }

    // --- orphan lifecycle -----------------------------------------------------------

    #[test]
    fn custody_tracks_from_never_through_held_to_settled() {
        let t = Timeline::build(
            &[
                step("a", 1000, 100, 1, 0, vec![]),
                step("b", 1000, 100, 1, 70, vec![]),
                step("c", 930, 0, 1, 0, vec![]),
            ],
            &manifest(),
            Policy::Pair,
        );
        let o = t.orphans.iter().find(|o| o.tok == Tok::Stable).unwrap();
        assert_eq!(o.state, Custody::Settled);
        assert_eq!(o.amount, 70);
        assert_eq!(o.since.as_deref(), Some("b"));
        assert_eq!(o.settled_at.as_deref(), Some("c"));
        assert!(o.notes.iter().all(|n| n.ok), "{:?}", o.notes);
    }

    #[test]
    fn settling_an_orphan_while_debt_remains_is_a_restriction_4_failure() {
        let t = Timeline::build(
            &[
                step("a", 1000, 100, 1, 70, vec![]),
                step("b", 930, 100, 1, 0, vec![]),
            ],
            &manifest(),
            Policy::Pair,
        );
        let o = t.orphans.iter().find(|o| o.tok == Tok::Stable).unwrap();
        assert!(o
            .notes
            .iter()
            .any(|n| !n.ok && n.text.to_string().contains("d_supply 100")));
    }

    // --- headroom -------------------------------------------------------------------

    #[test]
    fn gauges_read_utilisation_against_its_ceiling() {
        let s = step("x", 1000, 650, 1_000_000_000_000, 0, vec![]);
        let g = gauges(&s.b.snapshot, &manifest())
            .into_iter()
            .find(|g| g.tok == Tok::Stable)
            .unwrap();
        assert_eq!(g.util, 6_500_000); // 65%
        assert_eq!(g.max_util, 9_500_000);
        assert!(!g.over_util());
        assert_eq!(pct(g.util), "65.00%");
        assert_eq!(pct(g.max_util), "95.00%");
    }

    #[test]
    fn utilisation_above_its_ceiling_is_flagged() {
        let s = step("x", 1000, 990, 1_000_000_000_000, 0, vec![]);
        let g = gauges(&s.b.snapshot, &manifest()).remove(0);
        assert!(g.over_util());
        assert_eq!(g.util_frac(), 1.0);
    }

    #[test]
    fn an_unbounded_cap_is_named_rather_than_printed() {
        assert!(uncapped(i128::MAX));
        assert!(!uncapped(1_000_000));
        let g = Gauge {
            tok: Tok::Stable,
            util: 0,
            max_util: 9_500_000,
            supply: 5,
            cap: i128::MAX,
            decimals: 6,
        };
        assert_eq!(g.cap_frac(), 0.0);
        assert!(!g.over_cap());
    }

    #[test]
    fn bar_fills_proportionally_and_never_overflows() {
        assert_eq!(bar(0.0, 4), "░░░░");
        assert_eq!(bar(1.0, 4), "████");
        assert_eq!(bar(0.5, 4), "██░░");
        assert_eq!(bar(9.0, 4).chars().count(), 4);
    }

    // --- loss-event validation and composition --------------------------------------

    #[test]
    fn a_negative_default_amount_fails_the_flow() {
        // The audit control: corrupt an emitted amount and the row must move.
        let f = only_flow(&[
            step("a", 1000, 500, 1_000_000_000_000, 0, vec![]),
            step(
                "b",
                1000,
                400,
                999_000_000_000,
                0,
                vec![ev("defaulted_debt", i128v(-1))],
            ),
        ]);
        assert!(
            failures(&f)
                .iter()
                .any(|t| t.contains("malformed i128 amount")),
            "{:?}",
            failures(&f)
        );
    }

    #[test]
    fn setoff_and_default_composed_in_one_call_reconcile() {
        // ADR 0008 r7: one bad-debt call burns 10 b in `debt_setoff`, repays
        // 10 d and defaults the remaining 5. The default's supply rule must be
        // judged against what the setoff already explains, not the raw -10.
        let f = only_flow(&[
            step("a", 1000, 500, 1_000_000_000_000, 0, vec![]),
            step(
                "b",
                990,
                485,
                999_000_000_000,
                0,
                vec![
                    ev("debt_setoff", json!({"vec": [i128v(10), i128v(10)]})),
                    ev("defaulted_debt", i128v(5)),
                ],
            ),
        ]);
        assert!(failures(&f).is_empty(), "{:?}", failures(&f));
    }

    #[test]
    fn a_default_event_from_a_foreign_contract_explains_nothing() {
        let mut forged = ev("defaulted_debt", i128v(100));
        forged.contract = "CGONER".into();
        let t = Timeline::build(
            &[
                step("a", 1000, 500, 1_000_000_000_000, 0, vec![]),
                step("b", 1000, 400, 999_000_000_000, 0, vec![forged]),
            ],
            &manifest(),
            Policy::Pair,
        );
        let s = &t.rates[0];
        assert!(
            !s.points[1].defaulted,
            "a forged event must not excuse a rate fall"
        );
        assert!(
            s.notes
                .iter()
                .any(|n| !n.ok && n.text.to_string().contains("non-pool contract")),
            "{:?}",
            s.notes
        );
        assert!(
            s.notes
                .iter()
                .any(|n| !n.ok && n.text.to_string().contains("b_rate fell")),
            "{:?}",
            s.notes
        );
    }

    #[test]
    fn an_unobservable_reserve_asset_is_a_failure_not_a_quiet_skip() {
        let mut a = step("a", 1000, 500, 1_000_000_000_000, 0, vec![]);
        let mut b = step(
            "b",
            1000,
            400,
            999_000_000_000,
            0,
            vec![ev("defaulted_debt", i128v(100))],
        );
        for s in [&mut a, &mut b] {
            s.b.snapshot
                .reserves
                .get_mut(&Tok::Stable)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove("asset");
        }
        let f = only_flow(&[a, b]);
        assert!(
            failures(&f)
                .iter()
                .any(|t| t.contains("reserve asset not observable")),
            "{:?}",
            failures(&f)
        );
    }

    // --- shared loss admission (report/compare/timeline single grammar) ------

    /// `emits_pool_loss` is the sole cross-surface admission: it judges the
    /// step's own side, and a configured reserve must carry the event asset.
    fn admitting_manifest() -> PairManifest {
        let mut m = manifest();
        m.b.contracts.stable = ASSET.into();
        m
    }

    fn loss_rec(
        events: Vec<Event>,
        ok: bool,
        error: Option<ErrorKind>,
    ) -> crate::observe::StepRecord {
        let mut s = step("loss", 0, 0, 1_000_000_000_000, 0, vec![]);
        let stderr = match &error {
            Some(ErrorKind::Contract(c)) => format!("error: Error(Contract, #{c})"),
            _ => String::new(),
        };
        s.b.invocation = Some(fixture_inv(ok, None, error.clone(), events, &stderr));
        s.b
    }

    #[test]
    fn loss_admission_requires_a_success_the_pool_and_a_configured_positive_asset() {
        let m = admitting_manifest();
        let genuine = loss_rec(vec![ev("defaulted_debt", i128v(100))], true, None);
        assert!(
            emits_pool_loss(&genuine, &m.b),
            "genuine loss arms admission"
        );
        assert!(emits_named_loss(&genuine, "defaulted_debt", &m.b));
        assert!(!emits_named_loss(&genuine, "bad_debt", &m.b));

        let mut failed = loss_rec(
            vec![ev("defaulted_debt", i128v(100))],
            false,
            Some(ErrorKind::Contract(1)),
        );
        assert!(
            !emits_pool_loss(&failed, &m.b),
            "a failed call's events admit nothing"
        );
        failed = loss_rec(
            vec![ev("defaulted_debt", i128v(100))],
            true,
            Some(ErrorKind::Contract(1)),
        );
        assert!(
            !emits_pool_loss(&failed, &m.b),
            "ok-plus-error is contradictory evidence"
        );

        let zero = loss_rec(vec![ev("defaulted_debt", i128v(0))], true, None);
        assert!(!emits_pool_loss(&zero, &m.b), "a zero loss is not a loss");

        let mut foreign = ev("defaulted_debt", i128v(100));
        foreign.contract = "CGONER".into();
        assert!(
            !emits_pool_loss(&loss_rec(vec![foreign], true, None), &m.b),
            "a non-pool emit admits nothing"
        );

        let mut unconfigured = ev("defaulted_debt", i128v(100));
        unconfigured.topics[2] = json!({"address": "COTHERASSET"});
        assert!(
            !emits_pool_loss(&loss_rec(vec![unconfigured], true, None), &m.b),
            "an asset the side does not configure admits nothing"
        );

        let rec = loss_rec(vec![ev("bad_debt", i128v(100))], true, None);
        assert!(
            emits_named_loss(&rec, "bad_debt", &m.b),
            "stock routing is named-recognizable"
        );
        assert!(
            !emits_pool_loss(&rec, &m.b),
            "stock bad_debt is not fork loss creation"
        );
    }

    /// The pre-#20 contracts published setoff and default with the asset as the
    /// only address topic. That shape is malformed now: it must neither arm
    /// admission nor explain an accounting move.
    #[test]
    fn a_userless_setoff_or_default_is_malformed() {
        let m = admitting_manifest();
        for mut e in [
            ev("defaulted_debt", i128v(100)),
            ev("debt_setoff", json!({"vec": [i128v(10), i128v(10)]})),
        ] {
            e.topics.remove(1);
            let name = event_name(&e).unwrap().to_owned();
            let rec = loss_rec(vec![e.clone()], true, None);
            assert!(
                !emits_pool_loss(&rec, &m.b),
                "{name}: two topics must not arm admission"
            );
            let (losses, problems) = losses_for(Policy::Pair, &[e], ASSET, "CPOOL");
            assert!(losses.is_empty(), "{name}: two topics explain nothing");
            assert!(
                problems
                    .iter()
                    .any(|p| p.to_string().contains("malformed asset topics")),
                "{name}: {problems:?}"
            );
        }
    }

    #[test]
    fn a_liquidation_fill_must_not_waive_the_loss_residual() {
        // Real liq_fill_final records emit fill_auction beside defaulted_debt.
        // The fill transfers positions; it cannot excuse a debt that stops
        // reconciling by one unit. Identical snapshots, only the amount moves.
        let run = |defaulted: i128| {
            only_flow(&[
                step("a", 1000, 500, 1_000_000_000_000, 0, vec![]),
                step(
                    "liq_fill_final",
                    1000,
                    400,
                    999_000_000_000,
                    0,
                    vec![
                        ev("fill_auction", json!({"u32": 1})),
                        ev("defaulted_debt", i128v(defaulted)),
                    ],
                ),
            ])
        };
        let exact = run(100);
        assert!(failures(&exact).is_empty(), "{:?}", failures(&exact));
        let off_by_one = run(101);
        assert!(
            failures(&off_by_one)
                .iter()
                .any(|t| t.contains("Δd_supply residual 1")),
            "{:?}",
            failures(&off_by_one)
        );
    }

    #[test]
    fn a_failed_call_does_not_explain_a_supply_move() {
        let mut moved = step(
            "b",
            800,
            0,
            1_000_000_000_000,
            0,
            vec![ev("supply", i128v(200))],
        );
        moved.b.invocation = Some(fixture_inv(
            false,
            None,
            Some(ErrorKind::Contract(1)),
            vec![ev("supply", i128v(200))],
            "error: Error(Contract, #1)",
        ));
        let f = only_flow(&[step("a", 1000, 0, 1_000_000_000_000, 0, vec![]), moved]);
        assert!(
            failures(&f).iter().any(|t| t.contains("unexplained")),
            "{:?}",
            failures(&f)
        );
    }

    #[test]
    fn fork_vs_fork_judges_each_sides_own_loss_arithmetic() {
        // A forged amount on side A must fail even when side B is exact:
        // temporal judgment belongs to each side, never to B alone.
        let mut m = manifest();
        m.profile = Profile::ForkVsFork;
        let mut s = step(
            "liq_fill_final",
            1000,
            400,
            999_000_000_000,
            0,
            vec![ev("defaulted_debt", i128v(100))],
        );
        s.a.invocation = Some(fixture_inv(
            true,
            None,
            None,
            vec![ev("defaulted_debt", i128v(101))],
            "",
        ));
        let t = Timeline::build(
            &[step("a", 1000, 500, 1_000_000_000_000, 0, vec![]), s],
            &m,
            Policy::Pair,
        );
        let failed_a: Vec<&Flow> = t
            .flows
            .iter()
            .filter(|f| f.side == 'a' && f.failed())
            .collect();
        assert!(
            failed_a.iter().any(|f| failures(f)
                .iter()
                .any(|t| t.contains("Δd_supply residual 1"))),
            "A-side 101 against Δd -100 must fail: {:?}",
            t.flows
        );
        assert!(
            t.flows.iter().all(|f| f.side != 'b' || !f.failed()),
            "the exact B side stays clean: {:?}",
            t.flows
        );
    }
}
