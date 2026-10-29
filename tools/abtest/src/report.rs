//! Evidence directory (per-step JSON) and the Markdown report.
//!
//! Replays are authoritative: acceptance never trusts the stored failure
//! lists. Every load recomputes the per-side invariants from the snapshots
//! and the manifest's mint baseline, re-runs the shared expectation checker,
//! and re-runs the rejected-call storage and rate-assertion checks against the
//! captured baseline. Malformed or missing information is FAIL/INCOMPLETE,
//! never PASS.
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::compare::{summarize, Class, Diff};
use crate::manifest::{PairManifest, Profile, RunState, Tok};
use crate::observe::try_num;
use crate::observe::StepRecord;
use crate::scenario::{LabeledStep, Step};
use crate::stellar::{ErrorKind, Policy, Safe};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StepEvidence {
    pub scenario: String,
    pub index: usize,
    pub label: String,
    pub a: StepRecord,
    pub b: StepRecord,
    pub diff: Diff,
    /// Failures re-derived from the evidence itself on load (recomputed
    /// invariants, shared expectation checks, baseline/storage checks), each
    /// prefixed with the side it judges. Never trusted from disk: `recompute`
    /// clears and refills this on every load.
    #[serde(default)]
    pub findings: Vec<String>,
}

pub fn write_step(out: &Path, ev: &StepEvidence) -> anyhow::Result<()> {
    use std::io::Write;
    let parent = out.join("evidence").join(&ev.scenario);
    std::fs::create_dir_all(&parent)?;
    let dir = parent.join(format!("{:03}-{}", ev.index, ev.label));
    // Reserving the directory is exclusive even after a prior partial write:
    // retained evidence can never be truncated by a retry or a re-run.
    std::fs::create_dir(&dir)?;
    for (name, bytes) in [
        ("a.json", serde_json::to_vec_pretty(&ev.a)?),
        ("b.json", serde_json::to_vec_pretty(&ev.b)?),
        ("diff.json", serde_json::to_vec_pretty(&ev.diff)?),
    ] {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(name))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
    }
    std::fs::File::open(&dir)?.sync_all()?;
    std::fs::File::open(&parent)?.sync_all()?;
    Ok(())
}

/// Load one persisted step directory, re-classifying its diff under the pair
/// manifest's profile and tolerance. The stored violation strings survive as
/// the historical record; the acceptance verdict comes from `recompute`,
/// never from this list being empty — and a non-empty list is itself
/// re-derived there as a failure, whatever its text.
pub fn load_step_dir(
    dir: &Path,
    scenario: &str,
    m: &PairManifest,
    delta_dependent: bool,
    pool_loss_seen: bool,
    freeze_split_seen: bool,
) -> anyhow::Result<StepEvidence> {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let (index, label) = name
        .split_once('-')
        .ok_or_else(|| anyhow::anyhow!("invalid evidence directory {name}"))?;
    let index: usize = index
        .parse()
        .map_err(|_| anyhow::anyhow!("noncanonical evidence directory {name}"))?;
    anyhow::ensure!(
        name == format!("{index:03}-{label}"),
        "noncanonical evidence directory {name}"
    );
    let a: StepRecord = serde_json::from_str(&std::fs::read_to_string(dir.join("a.json"))?)?;
    let b: StepRecord = serde_json::from_str(&std::fs::read_to_string(dir.join("b.json"))?)?;
    anyhow::ensure!(
        a.label == label && b.label == label,
        "evidence label mismatch in {name}"
    );
    let diff = crate::compare::compare_step(
        m,
        label,
        &a,
        &b,
        delta_dependent,
        pool_loss_seen,
        freeze_split_seen,
    );
    Ok(StepEvidence {
        scenario: scenario.to_string(),
        index,
        label: label.to_string(),
        a,
        b,
        diff,
        findings: Vec::new(),
    })
}

/// Scenario dir names in replay order: `scenario_order` recorded in state.json when
/// present, else the canonical `scenario.rs` definition order restricted to observed
/// dirs; dirs neither knows about (ad-hoc, e.g. `operator`) sort last. Both the report
/// replay and the TUI disk-watch must use this, or the two surfaces can disagree with
/// each other and with the live runner's cross-step context flags.
pub fn scenario_replay_order(
    out: &Path,
    observed: &std::collections::BTreeSet<String>,
) -> Vec<String> {
    let mut order = match crate::manifest::RunState::load(out) {
        Ok(s) => s.scenario_order,
        Err(e) => {
            // A corrupt state.json silently degrades custody replay to
            // canonical order. Loudly so — a wrong classification is the
            // outcome this function exists to prevent.
            eprintln!("report: cannot read state.json scenario order ({e:#}); falling back to canonical scenario order");
            Vec::new()
        }
    };
    if order.is_empty() {
        order = crate::scenario::scenarios()
            .into_iter()
            .map(|s| s.name.to_string())
            .filter(|n| observed.contains(n))
            .collect();
    }
    for name in observed {
        if !order.iter().any(|o| o == name) {
            order.push(name.clone());
        }
    }
    order
}

pub fn load_steps(out: &Path, m: &PairManifest) -> anyhow::Result<Vec<StepEvidence>> {
    let delta: BTreeMap<&'static str, bool> = crate::scenario::scenarios()
        .into_iter()
        .flat_map(|s| s.steps.into_iter().map(|st| (st.label, st.delta_dependent)))
        .collect();
    let mut dirs = Vec::new();
    let root = out.join("evidence");
    if !root.exists() {
        return Ok(Vec::new());
    }
    for scenario in std::fs::read_dir(&root)? {
        let scenario = scenario?;
        for d in std::fs::read_dir(scenario.path())? {
            dirs.push((scenario.file_name().to_string_lossy().to_string(), d?));
        }
    }
    // Judge in execution order so the cumulative custody/freeze context
    // flags replay the same way the live runner held them. `scenario_order`
    // from state.json is authoritative; a pre-upgrade run has none, so fall
    // back to the canonical scenario definition order. Sorting by scenario
    // rank (then scenario name, then step index) keeps each scenario
    // contiguous — the cumulative pool-loss custody flag assumes contiguity.
    // Delta mode is per-step: each step judges with its own marking only,
    // never one latched for the rest of the scenario.
    let observed: BTreeSet<String> = dirs.iter().map(|(s, _)| s.clone()).collect();
    let order = scenario_replay_order(out, &observed);
    let rank = |name: &str| order.iter().position(|o| o == name).unwrap_or(usize::MAX);
    dirs.sort_by(|a, b| {
        (rank(&a.0), a.0.as_str(), a.1.file_name()).cmp(&(
            rank(&b.0),
            b.0.as_str(),
            b.1.file_name(),
        ))
    });
    let mut steps = Vec::new();
    let mut pool_loss_seen = false;
    let mut freeze_split_seen = false;
    for (scenario, d) in dirs {
        let name = d.file_name().to_string_lossy().to_string();
        let label = name
            .split_once('-')
            .map(|x| x.1)
            .unwrap_or(&name)
            .to_string();
        let step = load_step_dir(
            &d.path(),
            &scenario,
            m,
            delta.get(label.as_str()).copied().unwrap_or(false),
            pool_loss_seen,
            freeze_split_seen,
        )?;
        if scenario != crate::run::OPERATOR_SCENARIO {
            pool_loss_seen |= crate::timeline::emits_pool_loss(&step.a, &m.a)
                || crate::timeline::emits_pool_loss(&step.b, &m.b);
        }
        if label == crate::scenario::FREEZE_DOWNGRADE_PROBE {
            freeze_split_seen = true;
        }
        steps.push(step);
    }
    // Recompute the authoritative verdict inputs from the evidence itself,
    // reconciled against the caller-supplied pair manifest — the same
    // manifest the diff was classified under, so no second read can diverge.
    recompute(m, &mut steps);
    Ok(steps)
}

// ---------------------------------------------------------------------------
// Authoritative recomputation over loaded evidence
// ---------------------------------------------------------------------------

/// The scope a valid `run` records: scenario -> its exact full label list,
/// the scenario's declared steps followed by every trap probe.
pub fn canonical_scope() -> BTreeMap<String, Vec<String>> {
    crate::scenario::scenarios()
        .into_iter()
        .map(|s| {
            (
                s.name.to_string(),
                s.steps
                    .into_iter()
                    .map(|step| step.label.to_string())
                    .chain(
                        crate::scenario::traps()
                            .into_iter()
                            .map(|step| step.label.to_string()),
                    )
                    .collect(),
            )
        })
        .collect()
}

/// Labels that complete by waiting and produce no evidence directory.
fn wait_only(label: &str) -> bool {
    matches!(
        crate::scenario::current_step(label).map(|s| &s.step),
        Some(Step::WaitSeconds(_) | Step::WaitLedgersSince { .. })
    )
}

/// Requested labels as a set, or `None` when no scope was recorded.
pub fn requested_scope(st: &RunState) -> Option<BTreeSet<String>> {
    if st.requested.is_empty() {
        None
    } else {
        Some(st.requested.values().flatten().cloned().collect())
    }
}

/// Refill `findings` on every evidence record from the evidence itself:
/// cumulative mint totals from the manifest + successful mint events, the
/// observed-invariant set, the shared expectation checker, the captured
/// baseline, the rejected-call storage check, and a per-record count of the
/// retained stored violations (any text, empty strings included).
/// Idempotent: findings are cleared and re-derived, never accumulated.
pub fn recompute(m: &PairManifest, steps: &mut [StepEvidence]) {
    let mut mint_a: BTreeMap<Tok, i64> = BTreeMap::new();
    let mut mint_b: BTreeMap<Tok, i64> = BTreeMap::new();
    for s in steps.iter_mut() {
        s.findings.clear();
        if s.scenario == crate::run::OPERATOR_SCENARIO {
            continue;
        }
        let step = crate::scenario::current_step(&s.label);
        let mut f = Vec::new();
        recompute_side(m.profile, 'a', &m.a, &s.a, &mut mint_a, step, &mut f);
        recompute_side(m.profile, 'b', &m.b, &s.b, &mut mint_b, step, &mut f);
        s.findings = f;
    }
}

fn recompute_side(
    profile: Profile,
    side: char,
    m: &crate::manifest::SideManifest,
    r: &StepRecord,
    minted: &mut BTreeMap<Tok, i64>,
    step: Option<&'static LabeledStep>,
    f: &mut Vec<String>,
) {
    if !r.violations.is_empty() {
        f.push(
            Safe::error_chain(
                Policy::Pair,
                &anyhow::anyhow!(
                    "{side} retained failures: {} recorded violation(s)",
                    r.violations.len()
                ),
            )
            .to_string(),
        );
    }
    let inv = r.invocation.as_ref();
    // SEP-41 topics name the admin and recipient; only the emitting token
    // contract identifies supply. Accumulate before checking this snapshot.
    // Success with a contract error is no evidence; this is the valid-
    // success half of the shared rule check_expectation applies (for ok,
    // stellar::definite_outcome is true by its own first branch).
    if let Some(i) = inv.filter(|i| i.ok && i.error.is_none()) {
        for e in &i.events {
            if e.topics.first().and_then(|t| t["symbol"].as_str()) != Some("mint") {
                continue;
            }
            let Some(tok) = Tok::MOCK
                .iter()
                .copied()
                .find(|t| m.contracts.token(*t) == e.contract)
            else {
                continue;
            };
            // Authenticated emitter + mint tag + garbled shape is malformed
            // data, not an absent mint: fail loudly, never silently drop.
            if !sep41_mint_shape(&e.topics, &e.value) {
                f.push(
                    Safe::error_chain(
                        Policy::Pair,
                        &anyhow::anyhow!(
                            "{side} invariant: {} mint event is malformed (topics [symbol mint, address, address], single i128 value)",
                            tok.as_str()
                        ),
                    )
                    .to_string(),
                );
                continue;
            }
            let total = e
                .value
                .get("i128")
                .and_then(try_num)
                .and_then(|v| i64::try_from(v).ok())
                .filter(|v| *v >= 0)
                .and_then(|v| minted.get(&tok).copied().unwrap_or(0).checked_add(v));
            match total {
                Some(total) => {
                    minted.insert(tok, total);
                }
                None => f.push(
                    Safe::error_chain(
                        Policy::Pair,
                        &anyhow::anyhow!(
                            "{side} invariant: {} mint amount is invalid or cumulative supply overflows i64",
                            tok.as_str()
                        ),
                    )
                    .to_string(),
                ),
            }
        }
    }
    for v in crate::observe::invariants(
        Policy::Pair,
        m,
        &r.snapshot,
        minted,
        crate::run::invariant_policy(profile, side),
    ) {
        f.push(
            Safe::error_chain(Policy::Pair, &anyhow::anyhow!("{side} invariant: {v}")).to_string(),
        );
    }
    match step {
        Some(ls) => {
            for x in crate::run::check_expectation(
                Policy::Pair,
                profile,
                side,
                ls,
                inv,
                r.before.as_ref(),
            ) {
                f.push(
                    Safe::error_chain(
                        Policy::Pair,
                        &anyhow::anyhow!("{side} expected-check: {x}"),
                    )
                    .to_string(),
                );
            }
        }
        None => f.push(
            Safe::error_chain(
                Policy::Pair,
                &anyhow::anyhow!(
                    "{side} expected-check: label {} has no expectation in the current scenario list",
                    r.label
                ),
            )
            .to_string(),
        ),
    }
    if inv.is_some_and(|i| i.ok && i.error.is_some()) {
        f.push(
            Safe::error_chain(
                Policy::Pair,
                &anyhow::anyhow!(
                    "{side} contradictory invocation: a successful record cannot carry a contract error"
                ),
            )
            .to_string(),
        );
    }
    if inv.is_some_and(|i| i.ok) {
        if let Some(Step::NewAuction { user, .. }) = step.map(|s| &s.step) {
            if let Some(finding) =
                crate::observe::auction_observed(Policy::Pair, &r.snapshot, *user)
            {
                f.push(
                    Safe::error_chain(Policy::Pair, &anyhow::anyhow!("{side} {finding}"))
                        .to_string(),
                );
            }
        }
    }
    if let Some(Step::AssertRatesIncreased { asset }) = step.map(|s| &s.step) {
        let rate = |s: &crate::observe::Snapshot, field: &str| {
            s.reserves
                .get(asset)
                .and_then(|v| v.get("data"))
                .and_then(|d| d.get(field))
                .and_then(try_num)
        };
        match r.before.as_ref().map(|b| {
            (
                rate(b, "b_rate"),
                rate(b, "d_rate"),
                rate(&r.snapshot, "b_rate"),
                rate(&r.snapshot, "d_rate"),
            )
        }) {
            Some((Some(b0), Some(d0), Some(b1), Some(d1))) => {
                if b1 <= b0 {
                    f.push(
                        Safe::error_chain(
                            Policy::Pair,
                            &anyhow::anyhow!(
                                "{side} rates: asserted b_rate did not increase ({b0} -> {b1})"
                            ),
                        )
                        .to_string(),
                    );
                }
                if d1 <= d0 {
                    f.push(
                        Safe::error_chain(
                            Policy::Pair,
                            &anyhow::anyhow!(
                                "{side} rates: asserted d_rate did not increase ({d0} -> {d1})"
                            ),
                        )
                        .to_string(),
                    );
                }
            }
            Some(_) => f.push(
                Safe::error_chain(
                    Policy::Pair,
                    &anyhow::anyhow!(
                        "{side} rates: asserted rates missing/unparseable across baseline and after; the increase is uncertified"
                    ),
                )
                .to_string(),
            ),
            None => f.push(
                Safe::error_chain(
                    Policy::Pair,
                    &anyhow::anyhow!(
                        "{side} rates: no recorded pre-assert baseline; the rate increase is uncertified"
                    ),
                )
                .to_string(),
            ),
        }
    }
    if inv.is_some() && r.before.is_none() {
        f.push(
            Safe::error_chain(
                Policy::Pair,
                &anyhow::anyhow!(
                    "{side} baseline: no pre-invocation snapshot; state-change claims cannot be certified"
                ),
            )
            .to_string(),
        );
    }
    if matches!(step.map(|s| &s.step), Some(&Step::Trap(_))) || inv.is_some_and(|i| !i.ok) {
        match &r.before {
            Some(b) => {
                for d in crate::observe::no_state_change(Policy::Pair, b, &r.snapshot) {
                    f.push(
                        Safe::error_chain(
                            Policy::Pair,
                            &anyhow::anyhow!("{side} rejected-call storage: {d}"),
                        )
                        .to_string(),
                    );
                }
            }
            None => f.push(
                Safe::error_chain(
                    Policy::Pair,
                    &anyhow::anyhow!(
                        "{side} rejected-call storage: no baseline captured; the immutability claim is uncertified"
                    ),
                )
                .to_string(),
            ),
        }
    }
}

/// SEP-41 mint is exactly `[symbol mint, address from, address to]` with a
/// single-tag `i128` value. Enforced with the same strictness the timeline
/// loss grammar applies, so a garbled event can never certify supply.
fn sep41_mint_shape(topics: &[serde_json::Value], value: &serde_json::Value) -> bool {
    let tag = |t: &serde_json::Value, key: &str| {
        t.as_object()
            .is_some_and(|o| o.len() == 1 && o.get(key).is_some_and(|v| v.is_string()))
    };
    topics.len() == 3
        && tag(&topics[0], "symbol")
        && topics[0]["symbol"].as_str() == Some("mint")
        && tag(&topics[1], "address")
        && tag(&topics[2], "address")
        && value
            .as_object()
            .is_some_and(|o| o.len() == 1 && o.contains_key("i128"))
}

// ---------------------------------------------------------------------------
// Authoritative verdict
// ---------------------------------------------------------------------------

fn outcome(r: &StepRecord) -> String {
    match &r.invocation {
        None => "-".into(),
        Some(i) if i.ok => "ok".into(),
        Some(i) => format!(
            "{:?}",
            i.error
                .clone()
                .unwrap_or(crate::stellar::ErrorKind::Other(Safe::literal("?")))
        ),
    }
}

/// 0 = PASS, 2 = FAIL verdict. (1 is reserved for tool/infra failure.)
pub fn verdict_code(verdict: &str) -> i32 {
    if verdict == "PASS" {
        0
    } else {
        2
    }
}

/// The single authoritative analysis of a run dir: the report command and the
/// TUI render the same verdict from the same computation.
#[derive(Clone, Debug)]
pub struct Analysis {
    /// Evidence with `findings` recomputed.
    pub steps: Vec<StepEvidence>,
    pub ledger: Vec<Row>,
    /// `PASS`, `FAIL`, or `INCOMPLETE` (custody/legacy: not certifiable).
    pub verdict: String,
    pub expected: usize,
    pub violations: usize,
    pub invariant_failures: usize,
    pub check_failures: usize,
    pub ledger_failures: usize,
    pub timeline_violations: usize,
    pub integrity: Vec<String>,
}

pub fn analyze(out: &Path, m: &PairManifest, steps: &[StepEvidence]) -> Analysis {
    let mut steps = steps.to_vec();
    recompute(m, &mut steps);

    let mut integrity: Vec<String> = Vec::new();
    let state = if !out.join("state.json").exists() {
        integrity.push(
            "state.json is missing: the requested scope is unknown; completeness cannot be verified"
                .into(),
        );
        None
    } else {
        match RunState::load(out) {
            Ok(s) => Some(s),
            Err(e) => {
                integrity.push(
                    Safe::error_chain(
                        Policy::Pair,
                        &anyhow::anyhow!(
                            "state.json is unreadable ({e:#}): the requested scope is unknown; completeness cannot be verified"
                        ),
                    )
                    .to_string(),
                );
                None
            }
        }
    };
    integrity.extend(scope_integrity(state.as_ref(), &steps));
    integrity.extend(pending_integrity(out));
    if steps
        .iter()
        .all(|s| s.scenario == crate::run::OPERATOR_SCENARIO)
    {
        integrity
            .push("no judged (non-operator) evidence exists: there is nothing to certify".into());
    }

    let timeline = crate::timeline::Timeline::build(&steps, m, Policy::Pair);
    let timeline_violations = timeline.violations();
    for flow in timeline.flows.iter().filter(|f| f.failed()) {
        for step in steps
            .iter_mut()
            .filter(|s| s.scenario == flow.scenario && s.label == flow.label)
        {
            step.findings
                .extend(flow.notes.iter().filter(|n| !n.ok).map(|n| {
                    Safe::error_chain(
                        Policy::Pair,
                        &anyhow::anyhow!(
                            "{} timeline: {}: {}",
                            flow.side,
                            flow.tok.as_str(),
                            n.text
                        ),
                    )
                    .to_string()
                }));
        }
    }
    let scope = state.as_ref().and_then(requested_scope);
    let mut ledger = ledger_rows(m, &steps, scope.as_ref());
    let ledger_failures = ledger
        .iter()
        .filter(|r| matches!(r.status.as_str(), "FAIL" | "PARTIAL"))
        .count();

    let mut all_diffs: BTreeMap<String, Diff> = BTreeMap::new();
    let mut invariant_failures = 0usize;
    let mut check_failures = 0usize;
    for s in steps
        .iter()
        .filter(|s| s.scenario != crate::run::OPERATOR_SCENARIO)
    {
        all_diffs.insert(format!("{}/{}", s.scenario, s.label), s.diff.clone());
        invariant_failures +=
            s.a.violations
                .iter()
                .chain(&s.b.violations)
                .filter(|v| format!("{v}").starts_with("invariant:"))
                .count();
        let re_inv = s
            .findings
            .iter()
            .filter(|x| x.contains("invariant:"))
            .count();
        invariant_failures += re_inv;
        check_failures += s.findings.len() - re_inv;
    }
    let (expected, violations) = summarize(&all_diffs);

    ledger.insert(
        0,
        Row {
            clause: "run custody",
            what: "requested scope recorded and canonical; every requested step completed with evidence; no unresolved pending invocations",
            status: (if integrity.is_empty() { "PASS" } else { "INCOMPLETE" }).into(),
            evidence: if integrity.is_empty() {
                "scope recorded, complete, pending-free".into()
            } else {
                integrity.join("<br>")
            },
        },
    );

    let verdict = if violations > 0
        || invariant_failures > 0
        || check_failures > 0
        || ledger_failures > 0
        || timeline_violations > 0
    {
        "FAIL"
    } else if !integrity.is_empty() {
        "INCOMPLETE"
    } else {
        "PASS"
    };
    Analysis {
        steps,
        ledger,
        verdict: verdict.into(),
        expected,
        violations,
        invariant_failures,
        check_failures,
        ledger_failures,
        timeline_violations,
        integrity,
    }
}

/// Any remaining pending entry means an invocation whose outcome never became
/// durable: the run cannot be certified (ExecutionRepair custody contract).
fn pending_integrity(out: &Path) -> Vec<String> {
    let mut v = Vec::new();
    let root = out.join("pending");
    let Ok(scenarios) = std::fs::read_dir(&root) else {
        return v;
    };
    for sc in scenarios.flatten() {
        match std::fs::read_dir(sc.path()) {
            Ok(steps) => {
                for d in steps.flatten() {
                    v.push(
                        Safe::error_chain(
                            Policy::Pair,
                            &anyhow::anyhow!(
                                "unresolved pending step {}: the invocation outcome was never made durable",
                                d.path().display()
                            ),
                        )
                        .to_string(),
                    );
                }
            }
            Err(e) => v.push(
                Safe::error_chain(
                    Policy::Pair,
                    &anyhow::anyhow!(
                        "pending directory {} unreadable ({e}): custody state unknown",
                        sc.path().display()
                    ),
                )
                .to_string(),
            ),
        }
    }
    v
}

/// Requested-scope completeness: the recorded scope must be an isolated
/// subset of exactly one scenario family with its loss prerequisites present
/// (the same validity rule `select_scenarios` enforces at drive time —
/// cross-family contamination is never certifiable), every requested scenario
/// declared and carrying its scenario's exact canonical label list (so a
/// hand-truncated list can never shrink what completeness demands), every
/// requested label completed, waits proven by `completed`, non-wait labels
/// proven by evidence, and nothing judged that was not requested.
fn scope_integrity(state: Option<&RunState>, steps: &[StepEvidence]) -> Vec<String> {
    let mut v = Vec::new();
    let Some(st) = state else { return v };
    if st.requested.is_empty() {
        v.push(
            Safe::error_chain(
                Policy::Pair,
                &anyhow::anyhow!(
                    "no requested scope was recorded (pre-upgrade run): scope completeness cannot be verified and historical evidence is not re-certified"
                ),
            )
            .to_string(),
        );
        return v;
    }
    let names: Vec<&str> = st.requested.keys().map(|k| k.as_str()).collect();
    if crate::scenario::family_of(&names).is_none()
        || !crate::scenario::loss_prerequisites_ok(&names)
    {
        v.push(
            Safe::error_chain(
                Policy::Pair,
                &anyhow::anyhow!(
                    "requested scope {names:?} is not an isolated canonical family subset with its loss prerequisites (one family per fresh deployment); {}",
                    crate::scenario::SELECTION_GUIDANCE
                ),
            )
            .to_string(),
        );
    }
    let canon = canonical_scope();
    let mut dup_evidence: BTreeSet<(&str, &str)> = BTreeSet::new();
    let mut seen_evidence: BTreeSet<(&str, &str)> = BTreeSet::new();
    for s in steps {
        if s.scenario == crate::run::OPERATOR_SCENARIO {
            continue;
        }
        if !seen_evidence.insert((s.scenario.as_str(), s.label.as_str())) {
            dup_evidence.insert((s.scenario.as_str(), s.label.as_str()));
        }
    }
    for (s, l) in dup_evidence {
        v.push(
            Safe::error_chain(
                Policy::Pair,
                &anyhow::anyhow!(
                    "duplicate evidence directories for {s}/{l}: a completed label was executed twice"
                ),
            )
            .to_string(),
        );
    }
    for (s, list) in &st.completed {
        let mut seen = BTreeSet::new();
        for l in list {
            if !seen.insert(l) {
                v.push(
                    Safe::error_chain(
                        Policy::Pair,
                        &anyhow::anyhow!(
                            "scenario {s} completed label {l} more than once: a completed step was re-run"
                        ),
                    )
                    .to_string(),
                );
            }
        }
        if !st.requested.contains_key(s) && s.as_str() != crate::run::OPERATOR_SCENARIO {
            v.push(
                Safe::error_chain(
                    Policy::Pair,
                    &anyhow::anyhow!("scenario {s} records completions but was not requested"),
                )
                .to_string(),
            );
        }
    }
    for (s, labels) in &st.requested {
        let want = match canon.get(s) {
            None => {
                v.push(
                    Safe::error_chain(
                        Policy::Pair,
                        &anyhow::anyhow!("requested scenario {s} is not a declared scenario"),
                    )
                    .to_string(),
                );
                continue;
            }
            Some(w) => w,
        };
        if labels != want {
            v.push(
                Safe::error_chain(
                    Policy::Pair,
                    &anyhow::anyhow!(
                        "requested labels for scenario {s} differ from the canonical scenario plan: the scope was truncated or edited and is not certifiable"
                    ),
                )
                .to_string(),
            );
            continue;
        }
        let done: BTreeSet<&String> = st
            .completed
            .get(s)
            .map(|v| v.iter().collect())
            .unwrap_or_default();
        let present: BTreeSet<&str> = steps
            .iter()
            .filter(|x| &x.scenario == s)
            .map(|x| x.label.as_str())
            .collect();
        for label in want {
            let has_ev = present.contains(label.as_str());
            if !done.contains(label) {
                v.push(
                    Safe::error_chain(
                        Policy::Pair,
                        &anyhow::anyhow!("requested step {s}/{label} never completed"),
                    )
                    .to_string(),
                );
            } else if wait_only(label) {
                if has_ev {
                    v.push(
                        Safe::error_chain(
                            Policy::Pair,
                            &anyhow::anyhow!(
                                "wait-only step {s}/{label} unexpectedly carries evidence"
                            ),
                        )
                        .to_string(),
                    );
                }
            } else if !has_ev {
                v.push(
                    Safe::error_chain(
                        Policy::Pair,
                        &anyhow::anyhow!("completed step {s}/{label} has no evidence directory"),
                    )
                    .to_string(),
                );
            }
        }
        if let Some(done) = st.completed.get(s) {
            for l in done {
                if !want.iter().any(|w| w == l) {
                    v.push(
                        Safe::error_chain(
                            Policy::Pair,
                            &anyhow::anyhow!(
                                "completed label {s}/{l} is outside the requested scope"
                            ),
                        )
                        .to_string(),
                    );
                }
            }
        }
    }
    for x in steps
        .iter()
        .filter(|x| x.scenario != crate::run::OPERATOR_SCENARIO)
    {
        match st.requested.get(&x.scenario) {
            None => v.push(
                Safe::error_chain(
                    Policy::Pair,
                    &anyhow::anyhow!(
                        "evidence {}/{} is outside the requested scope",
                        x.scenario,
                        x.label
                    ),
                )
                .to_string(),
            ),
            Some(want) => {
                if !want.iter().any(|w| w == &x.label) {
                    v.push(
                        Safe::error_chain(
                            Policy::Pair,
                            &anyhow::anyhow!(
                                "evidence {}/{} is outside the requested scope",
                                x.scenario,
                                x.label
                            ),
                        )
                        .to_string(),
                    );
                }
            }
        }
    }
    v
}

// ---------------------------------------------------------------------------
// ADR invariant ledger: one row per clause, judged on the fork side (B; both
// sides under fork-vs-fork), with the observed evidence spelled out. Every
// recorded occurrence of a label is judged; missing observations are UNKNOWN
// (FAIL), never vacuous passes; `scope` distinguishes not-requested clauses
// from missing required checks.
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Row {
    pub clause: &'static str,
    pub what: &'static str,
    pub status: String,
    pub evidence: String,
}

/// Every non-operator record carrying this label — traps re-run after every
/// scenario, so the first occurrence must never certify later behavior.
fn find_all<'a>(steps: &'a [StepEvidence], label: &str) -> Vec<&'a StepEvidence> {
    steps
        .iter()
        .filter(|s| s.label == label && s.scenario != crate::run::OPERATOR_SCENARIO)
        .collect()
}

fn side_findings(s: &StepEvidence, side: char) -> Vec<&String> {
    let p = format!("{side} ");
    s.findings.iter().filter(|f| f.starts_with(&p)).collect()
}

/// Failures the ledger must count for one record: stored violations plus the
/// recomputed findings for that record's judged side.
fn rec_failures(s: &StepEvidence, side: char, r: &StepRecord) -> Vec<String> {
    r.violations
        .iter()
        .map(|v| v.to_string())
        .chain(side_findings(s, side).into_iter().cloned())
        .collect()
}

fn rec_ok(r: &StepRecord) -> bool {
    r.invocation
        .as_ref()
        .is_some_and(|i| i.ok && i.error.is_none())
}

fn rec_err(r: &StepRecord) -> Option<u32> {
    match r.invocation.as_ref().and_then(|i| i.error.clone()) {
        Some(ErrorKind::Contract(c)) => Some(c),
        _ => None,
    }
}

fn events(r: &StepRecord) -> Vec<String> {
    r.invocation
        .iter()
        .flat_map(|i| i.events.iter())
        .filter_map(|e| {
            e.topics
                .first()
                .and_then(|t| t["symbol"].as_str())
                .map(str::to_string)
        })
        .collect()
}

fn describe(r: &StepRecord) -> String {
    match r.invocation.as_ref() {
        None => "no invocation".into(),
        Some(i) if i.ok => format!("ok, events {:?}", events(r)),
        Some(i) => match &i.error {
            Some(ErrorKind::Contract(c)) => format!("Error(Contract, #{c})"),
            Some(ErrorKind::Auth) => "auth rejected".into(),
            Some(ErrorKind::Other(o)) => {
                format!(
                    "failed: {}",
                    format!("{o}").chars().take(80).collect::<String>()
                )
            }
            None => "failed".into(),
        },
    }
}

/// Object with every `keys` member present and itself an object; `None` marks
/// the observation missing/malformed (UNKNOWN), which callers must never
/// confuse with an observed-empty collection.
fn obj<'a>(v: &'a serde_json::Value, keys: [&str; 3]) -> Option<[&'a serde_json::Value; 3]> {
    let o = v.as_object()?;
    let mut out: [Option<&serde_json::Value>; 3] = [None, None, None];
    for (i, k) in keys.iter().enumerate() {
        out[i] = o.get(*k).filter(|v| v.is_object());
    }
    Some([out[0]?, out[1]?, out[2]?])
}

fn map_obj_empty(v: &serde_json::Value) -> Option<bool> {
    v.as_object().map(|o| o.is_empty())
}

/// Judge every recorded occurrence of one label on the fork side(s): expected
/// outcome `Ok` or `Err(code)`. Any failing occurrence fails the row; a label
/// outside the requested scope is NOT REQUESTED, distinct from NOT RUN.
fn judge(
    steps: &[StepEvidence],
    profile: Profile,
    label: &str,
    want_err: Option<u32>,
    scope: Option<&BTreeSet<String>>,
) -> (String, String) {
    let wanted = |label: &str| scope.map(|sc| sc.contains(label)).unwrap_or(true);
    let occ = find_all(steps, label);
    if occ.is_empty() {
        return if !wanted(label) {
            (
                "NOT REQUESTED".into(),
                format!("`{label}` is outside the requested scope"),
            )
        } else {
            ("NOT RUN".into(), format!("`{label}` not executed"))
        };
    }
    let sides: Vec<char> = match profile {
        Profile::StockVsFork => vec!['b'],
        Profile::ForkVsFork => vec!['a', 'b'],
    };
    let mut pass = true;
    let mut ev = Vec::new();
    for (n, s) in occ.iter().enumerate() {
        for tag in &sides {
            let r = if *tag == 'a' { &s.a } else { &s.b };
            let bad = rec_failures(s, *tag, r);
            let good = match want_err {
                Some(c) => rec_err(r) == Some(c),
                None => rec_ok(r),
            } && bad.is_empty();
            pass &= good;
            ev.push(
                Safe::error_chain(
                    Policy::Pair,
                    &anyhow::anyhow!(
                        "#{n} {tag}: {}{}",
                        describe(r),
                        if bad.is_empty() {
                            String::new()
                        } else {
                            format!("; failures {bad:?}")
                        }
                    ),
                )
                .to_string(),
            );
        }
    }
    let stock = if profile == Profile::StockVsFork {
        Safe::error_chain(
            Policy::Pair,
            &anyhow::anyhow!("; stock: {}", describe(&occ[0].a)),
        )
        .to_string()
    } else {
        String::new()
    };
    (
        (if pass { "PASS" } else { "FAIL" }).into(),
        Safe::error_chain(
            Policy::Pair,
            &anyhow::anyhow!(
                "`{label}` {} occurrence(s): {}{}",
                occ.len(),
                ev.join(" / "),
                stock
            ),
        )
        .to_string(),
    )
}

type JudgedSides<'a> = Vec<(char, &'a StepRecord)>;

/// One step that claims a pool loss, the judged side records, and the
/// sides whose claims are authenticated against the manifest.
type ClaimedLoss<'a> = (&'a StepEvidence, JudgedSides<'a>, Vec<char>);

fn fork_records(steps: &[StepEvidence], profile: Profile) -> Vec<&StepRecord> {
    steps
        .iter()
        .filter(|s| s.scenario != crate::run::OPERATOR_SCENARIO)
        .flat_map(|s| match profile {
            Profile::StockVsFork => vec![&s.b],
            Profile::ForkVsFork => vec![&s.a, &s.b],
        })
        .collect()
}

/// The records the ledger judges for one step under the active profile: the
/// fork side only for stock-vs-fork, both sides for fork-vs-fork.
fn judged_sides(s: &StepEvidence, profile: Profile) -> JudgedSides<'_> {
    match profile {
        Profile::StockVsFork => vec![('b', &s.b)],
        Profile::ForkVsFork => vec![('a', &s.a), ('b', &s.b)],
    }
}

pub fn ledger_rows(
    m: &PairManifest,
    steps: &[StepEvidence],
    scope: Option<&BTreeSet<String>>,
) -> Vec<Row> {
    let profile = m.profile;
    let mut rows: Vec<Row> = Vec::new();
    let mut push =
        |clause: &'static str, what: &'static str, (status, evidence): (String, String)| {
            rows.push(Row {
                clause,
                what,
                status,
                evidence,
            });
        };
    let wanted = |label: &str| scope.map(|sc| sc.contains(label)).unwrap_or(true);
    let j = |label: &str, want: Option<u32>| judge(steps, profile, label, want, scope);
    let all = |labels: &[(&str, Option<u32>)]| -> (String, String) {
        let parts: Vec<(String, String)> = labels
            .iter()
            .map(|lw| judge(steps, profile, lw.0, lw.1, scope))
            .collect();
        let visible: Vec<&(String, String)> =
            parts.iter().filter(|p| p.0 != "NOT REQUESTED").collect();
        let status = if visible.is_empty() {
            "NOT REQUESTED"
        } else if visible.iter().any(|p| p.0 == "FAIL") {
            "FAIL"
        } else if visible.iter().all(|p| p.0 == "PASS") {
            "PASS"
        } else {
            "NOT RUN"
        };
        (
            status.into(),
            parts
                .into_iter()
                .map(|(_, e)| e)
                .collect::<Vec<_>>()
                .join("<br>"),
        )
    };

    push("ADR 0008 r1", "Status 4 is absorbing and backstop-independent: set succeeds, every exit rejects, withdraw stays open; frozen repay is not exercised by the suite",
        all(&[("freeze_set_status_4", None), ("freeze_update_status_rejected", Some(1204)), ("freeze_set_status_0_rejected", Some(1204)), ("freeze_supply_rejected", Some(1206)), ("freeze_withdraw_allowed", None)]));
    // r2: trap plus bstop_rate == 0 in every fork snapshot. The rate is read
    // from `config.bstop_rate` — the reserve's `b_rate` is the 12-dp interest
    // index and starts at 1e12, so it can never satisfy this clause.
    let (s2, e2) = j("trap_pool_update_pool", Some(1200));
    let rates: Vec<Option<i128>> = fork_records(steps, profile)
        .iter()
        .map(|r| try_num(&r.snapshot.config["bstop_rate"]))
        .collect();
    let (st, ev2) = if s2 == "NOT REQUESTED" {
        ("NOT REQUESTED".to_string(), e2)
    } else if rates.is_empty() {
        ("NOT RUN".to_string(), "no fork snapshots".to_string())
    } else if rates.iter().any(|v| v.is_none()) {
        (
            "FAIL".to_string(),
            format!("{e2}<br>bstop_rate unreadable in a fork snapshot"),
        )
    } else {
        let zero = rates.iter().all(|v| *v == Some(0));
        let st = if s2 == "PASS" && zero {
            "PASS"
        } else if s2 == "NOT RUN" && zero {
            "PARTIAL"
        } else {
            "FAIL"
        };
        let ev = format!(
            "{e2}<br>bstop_rate observed values: {:?} over {} snapshots",
            rates.iter().collect::<BTreeSet<_>>(),
            rates.len()
        );
        (st.to_string(), ev)
    };
    push(
        "ADR 0008 r2",
        "Zero take: `update_pool` traps; `bstop_rate` is 0 in every observed snapshot",
        (st, ev2),
    );
    push("ADR 0008 r3", "Sealed configuration after activation: modifying an existing reserve and adding a new one both reject with InvalidReserveMetadata",
        all(&[("sealed_queue_modify", Some(1202)), ("sealed_queue_new", Some(1202))]));
    let (s4, e4) = j("direct_default_gulp_with_debt", Some(1200));
    push(
        "ADR 0008 r4",
        "No surplus path: `gulp` rejects with BadRequest while the reserve has debt (the post-zero `orphan_settled` settle is not exercised by the suite)",
        (s4, e4),
    );
    let r5 = all(&[
        ("oracle_baseline_borrow", None),
        ("oracle_borrow_future_price", Some(1210)),
        ("oracle_borrow_stale_price", Some(1210)),
        ("oracle_borrow_zero_price", Some(1210)),
        ("oracle_recovered_borrow", None),
    ]);
    push("ADR 0008 r5", "Strict price observations: future-dated, stale > 24h and nonpositive all reject, and a good observation borrows again afterwards",
        (r5.0, format!("{}<br>decimals != 7 is not probed live: it needs `set_data`, which would rewrite the oracle's asset table mid-run; covered by pool unit tests", r5.1)));
    push(
        "ADR 0008 r6",
        "No flash loans: `flash_loan` rejects before any effect",
        j("trap_pool_flash_loan", Some(1200)),
    );
    // r7: judge every recorded occurrence of the loss probe. The shared
    // timeline grammar authenticates a positive, correctly sourced pool loss
    // event for a configured asset; malformed or foreign evidence never passes.
    let probed = !find_all(steps, "liq_fill_final").is_empty();
    // Select every *claimed* loss occurrence first: a record that names a
    // fork-loss symbol is judged no matter how malformed, and authentication
    // (below) decides whether it passes. Dropping invalid claims here would
    // let a good early occurrence hide a forged later one.
    let mut loss: Vec<ClaimedLoss> = Vec::new();
    let mut saw_bad_debt = false;
    for s in steps
        .iter()
        .filter(|s| s.scenario != crate::run::OPERATOR_SCENARIO)
    {
        let rs = judged_sides(s, profile);
        if rs
            .iter()
            .any(|(_, r)| events(r).iter().any(|e| e == "bad_debt"))
        {
            saw_bad_debt = true;
        }
        if !rs.iter().any(|(_, r)| crate::timeline::claims_pool_loss(r)) {
            continue;
        }
        let authenticated: Vec<char> = rs
            .iter()
            .filter(|(tag, r)| {
                let side = if *tag == 'a' { &m.a } else { &m.b };
                crate::timeline::emits_pool_loss(r, side)
            })
            .map(|(tag, _)| *tag)
            .collect();
        loss.push((s, rs, authenticated));
    }
    let r7 = if !probed {
        if wanted("liq_fill_final") {
            ("NOT RUN".into(), "`liq_fill_final` not executed".into())
        } else {
            (
                "NOT REQUESTED".into(),
                "`liq_fill_final` is outside the requested scope".into(),
            )
        }
    } else if loss.is_empty() {
        (
            "FAIL".into(),
            "`liq_fill_final` executed but no `defaulted_debt`, `debt_setoff` or `collateral_orphaned` event was claimed on any judged step".into(),
        )
    } else {
        let last = loss.last().expect("loss is non-empty");
        let custody_supply = last
            .0
            .b
            .snapshot
            .positions
            .get("pool")
            .and_then(|p| p.get("supply"))
            .and_then(|v| v.as_object())
            .map(|o| o.len());
        let backstop_clean = loss.iter().all(|(_, rs, _)| {
            rs.iter().all(|(_, r)| {
                r.snapshot
                    .positions
                    .get("backstop")
                    .and_then(|bs| obj(bs, ["collateral", "liabilities", "supply"]))
                    .map(|[c, l, u]| [c, l, u].iter().all(|v| map_obj_empty(v) == Some(true)))
                    .unwrap_or(false)
            })
        });
        let pass = loss.iter().all(|(s, rs, auth)| {
            // Every judged side must itself carry the authenticated loss, and
            // must independently be a clean success: a side that forged or
            // omitted its loss may not escape judgment by dropping out.
            rs.iter().all(|(tag, r)| {
                auth.contains(tag) && rec_ok(r) && rec_failures(s, *tag, r).is_empty()
            })
        }) && !saw_bad_debt
            && backstop_clean
            && custody_supply.is_some();
        let labels: Vec<&str> = loss.iter().map(|(s, _, _)| s.label.as_str()).collect();
        (
            (if pass { "PASS" } else { "FAIL" }).to_string(),
            format!(
                "`defaulted_debt` authenticated across all {} judged step(s) {:?}; custody supply entries after `{}`: {custody_supply:?}; judged sides' backstop clean: {backstop_clean}; fork: {}; stock: {}",
                loss.len(),
                labels,
                last.0.label,
                describe(&last.0.b),
                describe(&last.0.a)
            ),
        )
    };
    push("ADR 0008 r7", "Direct residual default: `defaulted_debt` (and `debt_setoff`/`collateral_orphaned` when applicable), never `bad_debt`; residual collateral parked in pool custody; backstop never takes debt", r7);
    push(
        "ADR 0011",
        "Six backstop emissions exports trap with BadRequest (1000) and leave the captured state untouched (baseline-to-after, recomputed on replay)",
        all(&[
            ("trap_backstop_distribute", Some(1000)),
            ("trap_backstop_gulp_emissions", Some(1000)),
            ("trap_backstop_add_reward", Some(1000)),
            ("trap_backstop_remove_reward", Some(1000)),
            ("trap_backstop_claim", Some(1000)),
            ("trap_backstop_drop", Some(1000)),
        ]),
    );
    push(
        "ADR 0008 r7 / ADR 0011",
        "Pool `set_emissions_config` traps; pool emissions map never exists",
        {
            let (s, e) = j("trap_pool_set_emissions_config", Some(1200));
            // Absence must be OBSERVED: the raw row must be present and null.
            // A missing key proves nothing about storage. With no fork snapshot
            // at all the clause is simply unobserved (INCOMPLETE), never a FAIL.
            let recs = fork_records(steps, profile);
            let absent = !recs.is_empty()
                && recs.iter().all(|r| {
                    r.snapshot
                        .raw
                        .get("pool_emis")
                        .map(|v| v.is_none())
                        .unwrap_or(false)
                });
            let status = if s == "NOT REQUESTED" {
                "NOT REQUESTED".into()
            } else if recs.is_empty() {
                "NOT RUN".into()
            } else if s == "PASS" && absent {
                "PASS".into()
            } else if s == "NOT RUN" && absent {
                "PARTIAL".into()
            } else {
                "FAIL".into()
            };
            (
                status,
                format!("{e}<br>PoolEmis observed absent (raw row present and null) in every fork snapshot: {absent}"),
            )
        },
    );
    // ADR 9 terminal predicates — each claim needs an actual observation.
    let recs = fork_records(steps, profile);
    let custody_clean = !recs.is_empty()
        && recs.iter().all(|r| {
            r.snapshot
                .positions
                .get("pool")
                .and_then(|p| obj(p, ["collateral", "liabilities", "supply"]))
                .map(|[c, l, _]| map_obj_empty(c) == Some(true) && map_obj_empty(l) == Some(true))
                .unwrap_or(false)
        });
    let credit_zero = !recs.is_empty()
        && recs.iter().all(|r| {
            Tok::RESERVES.iter().all(|t| {
                r.snapshot
                    .reserves
                    .get(t)
                    .and_then(|v| v.get("data"))
                    .and_then(|d| d.get("backstop_credit"))
                    .and_then(try_num)
                    == Some(0)
            })
        });
    let rz_empty = !recs.is_empty()
        && recs.iter().all(|r| {
            r.snapshot
                .reward_zone
                .as_array()
                .map(|a| a.is_empty())
                .unwrap_or(false)
        });
    push("ADR 0009 drained predicates", "Fork-side drained subset: pool custody has no collateral/liabilities (observed, not absent); `backstop_credit == 0` for every reserve; reward zone observed empty; emissions observed absent (above). Judged over the observed fork snapshots only, not a full drained migration.",
        ((if recs.is_empty() { "NOT RUN" } else if custody_clean && credit_zero && rz_empty { "PASS" } else { "FAIL" }).into(),
         format!("custody clean (observed): {custody_clean}; backstop_credit zero for all reserves (observed): {credit_zero}; reward_zone observed empty array: {rz_empty}; over {} snapshots", recs.len())));
    let inv_fail: Vec<String> = steps
        .iter()
        .filter(|s| s.scenario != crate::run::OPERATOR_SCENARIO)
        .flat_map(|s| {
            judged_sides(s, profile)
                .into_iter()
                .flat_map(move |(tag, r)| rec_failures(s, tag, r).into_iter())
                .filter(|x| x.contains("invariant:"))
        })
        .collect();
    push("ADR 0008 §6 accounting", "Per step, recomputed from the snapshots and the manifest + successful mint events: `d_supply == Σ liabilities`, `b_supply == Σ supply+collateral+custody`, `b_rate >= 0`, `d_rate > 0`, token conservation, backstop share/token conservation",
        ((if recs.is_empty() { "NOT RUN" } else if inv_fail.is_empty() { "PASS" } else { "FAIL" }).into(),
         format!("{} snapshots, recomputed invariant failures: {}", recs.len(), if inv_fail.is_empty() { "none".into() } else { inv_fail.iter().take(5).cloned().collect::<Vec<_>>().join("; ") })));
    let stress_labels: Vec<String> = crate::scenario::scenarios()
        .into_iter()
        .find(|s| s.name == "stress")
        .map(|s| s.steps.iter().map(|x| x.label.to_string()).collect())
        .unwrap_or_default();
    let stress_wanted = scope
        .map(|sc| stress_labels.iter().any(|l| sc.contains(l)))
        .unwrap_or(true);
    let stress: Vec<&StepEvidence> = steps
        .iter()
        .filter(|s| s.scenario == "stress" && s.scenario != crate::run::OPERATOR_SCENARIO)
        .collect();
    let stress_status = if !stress_wanted {
        "NOT REQUESTED"
    } else if stress.is_empty() {
        "NOT RUN"
    } else if stress.iter().all(|s| {
        judged_sides(s, profile).into_iter().all(|(tag, r)| {
            rec_failures(s, tag, r)
                .iter()
                .all(|x| !x.contains("invariant:"))
        })
    }) {
        "PASS"
    } else {
        "FAIL"
    };
    push(
        "stress",
        "Seeded mixed operations across actors/reserves with invariants after every step",
        (
            stress_status.into(),
            format!("{} stress steps", stress.len()),
        ),
    );
    // Retained historical findings are failure evidence, not decoration:
    // any stored violation string on canonical evidence — either side, any
    // text, empty strings included — must block certification and stay
    // attributable here, independent of whether a clause row judges the
    // label. Operator evidence is disclosed elsewhere, never judged.
    let canonical: Vec<&StepEvidence> = steps
        .iter()
        .filter(|s| s.scenario != crate::run::OPERATOR_SCENARIO)
        .collect();
    let retained: Vec<String> = canonical
        .iter()
        .flat_map(|s| {
            [('a', &s.a), ('b', &s.b)]
                .into_iter()
                .filter(|(_, r)| !r.violations.is_empty())
                .map(move |(tag, r)| {
                    format!(
                        "{tag} {}/{}: {} retained violation(s)",
                        s.scenario,
                        s.label,
                        r.violations.len()
                    )
                })
        })
        .collect();
    push(
        "retained canonical failures",
        "Derived on replay from the immutable stored violation lists: every retained violation on canonical evidence (either side, any text, empty strings included) is a failure that blocks certification; an empty stored list proves nothing",
        (
            if canonical.is_empty() {
                "NOT RUN".to_string()
            } else if retained.is_empty() {
                "PASS".to_string()
            } else {
                "FAIL".to_string()
            },
            if retained.is_empty() {
                "no canonical record carries retained violations".to_string()
            } else {
                retained.join("<br>")
            },
        ),
    );

    rows
}

fn ledger(rows: &[Row]) -> String {
    let mut md = String::from("## ADR invariant ledger\n\nJudged on the fork side (B); under fork-vs-fork both sides must satisfy each row. Rows are recomputed on replay: stored failure lists are evidence, never acceptance proof.\n\n| Clause | Property | Status | Evidence |\n|---|---|---|---|\n");
    for r in rows {
        md.push_str(&format!(
            "| {} | {} | **{}** | {} |\n",
            r.clause,
            r.what,
            r.status,
            r.evidence.replace('|', "\\|").replace('\n', " ")
        ));
    }
    md.push('\n');
    md
}

pub fn write_markdown(
    out: &Path,
    m: &PairManifest,
    steps: &[StepEvidence],
) -> anyhow::Result<String> {
    let a = analyze(out, m, steps);
    let steps = &a.steps;
    let mut md = String::new();
    md.push_str("# precept-abtest report\n\n");
    md.push_str(&format!(
        "Profile: `{:?}`. Network passphrase: `{}`.\n\n",
        m.profile, m.net.passphrase
    ));
    md.push_str("| Side | Label | Pool | Backstop | Pool wasm | Backstop wasm | Deploy ledger |\n|---|---|---|---|---|---|---|\n");
    for (tag, s) in [("A", &m.a), ("B", &m.b)] {
        md.push_str(&format!(
            "| {tag} | {} | `{}` | `{}` | `{}` | `{}` | {} |\n",
            s.label,
            s.contracts.pool,
            s.contracts.backstop,
            s.wasm.get("pool").cloned().unwrap_or_default(),
            s.wasm.get("backstop").cloned().unwrap_or_default(),
            s.deploy_ledger
        ));
    }
    md.push_str("\nIdentities: ");
    md.push_str(
        &m.identities
            .iter()
            .map(|(w, a)| format!("{}=`{a}`", w.as_str()))
            .collect::<Vec<_>>()
            .join(", "),
    );
    md.push_str("\n\n");

    let mut by_scenario: BTreeMap<&str, Vec<&StepEvidence>> = BTreeMap::new();
    for s in steps {
        by_scenario.entry(&s.scenario).or_default().push(s);
    }
    for (name, list) in &by_scenario {
        let is_operator = *name == crate::run::OPERATOR_SCENARIO;
        md.push_str(&format!("## {name}\n\n"));
        if is_operator {
            md.push_str("Fired by an operator, not by the scenario list. These mutate chain state and are recorded for honesty, but are excluded from the verdict: a deliberate one-sided action desyncs the pair by design.\n\n");
        }
        md.push_str("| # | Step | A | B | Class | Note |\n|---|---|---|---|---|---|\n");
        for s in list {
            let worst = s.diff.iter().fold("Match", |w, (_, c)| match c {
                Class::Violation(_) => "Violation",
                Class::Expected(_) if w != "Violation" => "Expected",
                _ => w,
            });
            let note: Vec<String> = s
                .diff
                .iter()
                .map(|(p, c)| match c {
                    Class::Expected(x) | Class::Violation(x) => format!("{p}: {x}"),
                    Class::Match => String::new(),
                })
                .filter(|x| !x.is_empty())
                .take(3)
                .collect();
            let inv: Vec<String> =
                s.a.violations
                    .iter()
                    .map(|v| format!("A {v}"))
                    .chain(s.b.violations.iter().map(|v| format!("B {v}")))
                    .chain(s.findings.iter().map(|v| format!("recheck {v}")))
                    .collect();
            md.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} |\n",
                s.index,
                s.label,
                outcome(&s.a),
                outcome(&s.b),
                worst,
                note.iter()
                    .chain(inv.iter())
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("; ")
                    .replace('|', "\\|")
            ));
        }
        md.push('\n');
    }
    md.push_str(&ledger(&a.ledger));
    if !a.integrity.is_empty() {
        md.push_str("## Integrity\n\n");
        for reason in &a.integrity {
            md.push_str(&format!("- {reason}\n"));
        }
        md.push('\n');
    }
    md.push_str("## Verdict\n\n");
    md.push_str(&format!(
        "| Steps | Expected divergences | Violations | Invariant failures | Check failures | Ledger failures | Timeline failures | Verdict |\n|---|---|---|---|---|---|---|---|\n| {} | {} | {} | {} | {} | {} | {} | **{}** |\n\n",
        steps.len(),
        a.expected,
        a.violations,
        a.invariant_failures,
        a.check_failures,
        a.ledger_failures,
        a.timeline_violations,
        a.verdict
    ));
    let n_operator = steps
        .iter()
        .filter(|s| s.scenario == crate::run::OPERATOR_SCENARIO)
        .count();
    if n_operator > 0 {
        md.push_str(&format!(
            "{n_operator} operator action(s) were performed during this run and are excluded from the verdict; see the `operator` section above.\n\n"
        ));
    }
    md.push_str(&format!(
        "Numeric leaves compare with the profile's relative tolerance ({:e}; interest accrues per second, so the two sides are never observed at one timestamp); a real divergence below that is masked. Steps marked delta-dependent exempt only auction `bid`/`lot` amounts — every other leaf, including reserve amounts, is still judged numerically.\n",
        crate::compare::tolerance(m.profile)
    ));
    md.push_str("Verdicts: PASS = every recomputed predicate holds over a complete recorded scope. FAIL = a judged predicate failed. INCOMPLETE = custody is not certifiable (missing/legacy scope, missing baselines, unresolved pending steps); never treat it as a pass.\n");
    std::fs::write(out.join("report.md"), &md)?;
    Ok(a.verdict)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{Contracts, ReserveRef, SideManifest, Who};
    use crate::observe::Snapshot;
    use crate::stellar::{Invocation, Net, RawEntry};
    use serde_json::{json, Value};
    use std::path::PathBuf;

    fn observed(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| n.to_string()).collect()
    }

    /// A pre-upgrade run has no recorded scenario order; replay must fall back
    /// to the canonical scenario.rs definition order — not directory-name order.
    #[test]
    fn replay_order_falls_back_to_canonical_scenario_order() {
        let dir = std::env::temp_dir().join(format!("abtest-order-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let order = scenario_replay_order(
            &dir,
            &observed(&["interest", "lending", "operator", "zzz_adhoc"]),
        );
        assert_eq!(
            order,
            vec!["lending", "interest", "operator", "zzz_adhoc"],
            "canonical first (definition order), unknown dirs last"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// state.json's scenario_order is authoritative when present.
    #[test]
    fn recorded_scenario_order_wins() {
        let dir = std::env::temp_dir().join(format!("abtest-order2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let st = crate::manifest::RunState {
            scenario_order: vec!["interest".into(), "lending".into()],
            ..Default::default()
        };
        st.save(&dir).unwrap();
        let order = scenario_replay_order(&dir, &observed(&["lending", "interest"]));
        assert_eq!(order, vec!["interest", "lending"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    use crate::observe::StepRecord;

    fn snap() -> Snapshot {
        clean_snap()
    }

    fn rec(side: &str, invocation: Option<Invocation>, violations: Vec<&str>) -> StepRecord {
        StepRecord {
            // loader-valid: must equal the evidence dir's label when persisted
            label: "direct_default_bad_debt".into(),
            side: side.into(),
            invocation,
            before: Some(snap()),
            snapshot: snap(),
            violations: serde_json::from_value::<Vec<crate::stellar::Safe>>(serde_json::json!(
                violations
            ))
            .unwrap(),
        }
    }

    fn inv(ok: bool, events: &[&str]) -> Invocation {
        let events: Vec<crate::stellar::Event> = events
            .iter()
            .map(|e| crate::stellar::Event {
                contract: "C".into(),
                topics: vec![json!({"symbol": e})],
                value: Value::Null,
            })
            .collect();
        serde_json::from_value(json!({
            "ok": ok,
            "value": null,
            "error": null,
            "events": serde_json::to_value(&events).unwrap(),
            "stderr": "",
        }))
        .unwrap()
    }

    fn ok_inv() -> Invocation {
        serde_json::from_value(json!({
            "ok": true,
            "value": null,
            "error": null,
            "events": [],
            "stderr": "",
        }))
        .unwrap()
    }

    fn err_inv(code: u32) -> Invocation {
        serde_json::from_value(json!({
            "ok": false,
            "value": null,
            "error": {"Contract": code},
            "events": [],
            "stderr": format!("error: Error(Contract, #{code})"),
        }))
        .unwrap()
    }

    /// Frozen contract: stored verdicts are history. `load_step_dir` must
    /// preserve every stored line untouched — expectation lines the current
    /// labels/profile no longer judge included — because the acceptance
    /// verdict comes from `recompute`, and an empty stored list is never
    /// proof of anything (other tests pin the re-derived findings).
    #[test]
    fn stored_expectation_lines_survive_as_history() {
        let dir = std::env::temp_dir().join(format!("abtest-keep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let step_dir = dir
            .join("evidence")
            .join("default")
            .join("010-direct_default_bad_debt");
        std::fs::create_dir_all(&step_dir).unwrap();
        let write = |f: &str, r: StepRecord| {
            std::fs::write(step_dir.join(f), serde_json::to_string(&r).unwrap()).unwrap()
        };
        write(
            "a.json",
            rec(
                "a",
                Some(inv(true, &[])),
                vec![
                    "expected Ok, got Err(Contract(#1200))",
                    "invariant: b_rate decreased",
                ],
            ),
        );
        write(
            "b.json",
            rec(
                "b",
                Some(inv(false, &[])),
                vec!["expected Err(Contract(#1200)), got Ok"],
            ),
        );
        std::fs::write(step_dir.join("diff.json"), "[]").unwrap();
        let s = load_step_dir(&step_dir, "default", &fx_manifest(), false, false, false).unwrap();
        assert_eq!(
            s.a.violations
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<String>>(),
            vec![
                "expected Ok, got Err(Contract(#1200))",
                "invariant: b_rate decreased"
            ]
        );
        assert_eq!(
            s.b.violations
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<String>>(),
            vec!["expected Err(Contract(#1200)), got Ok"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// RB4-1: palette re-runs under `operator` must never become ledger subjects.
    #[test]
    fn operator_reruns_never_become_ledger_subjects() {
        let operator_step = StepEvidence {
            scenario: crate::run::OPERATOR_SCENARIO.into(),
            index: 12,
            label: "liq_fill_final".into(),
            a: rec("a", Some(inv(true, &[])), vec![]),
            b: rec("b", Some(inv(true, &["defaulted_debt"])), vec![]),
            diff: vec![],
            findings: vec![],
        };
        let rows = ledger_rows(&fx_manifest(), &[operator_step], None);
        let r7 = rows.iter().find(|r| r.clause.ends_with("r7")).unwrap();
        assert_eq!(
            r7.status, "NOT RUN",
            "operator evidence cannot probe r7: {}",
            r7.evidence
        );
    }

    // ---------------------------------------------------------------------
    // Custody + recomputation regressions (audit false-pass controls)
    // ---------------------------------------------------------------------

    fn fx_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("abtest-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn fx_side(label: &str) -> SideManifest {
        SideManifest {
            label: label.into(),
            deploy_ledger: 1,
            wasm: BTreeMap::new(),
            contracts: Contracts {
                blnd: "CBLND".into(),
                usdc: "CUSDC".into(),
                weth: "CWETH".into(),
                xlm: "CXLM".into(),
                stable: "CSTBL".into(),
                lp: "CLP".into(),
                emitter: "CEMIT".into(),
                backstop: "CBSTP".into(),
                pool_factory: "CPF".into(),
                oracle: "CORC".into(),
                pool: "CPOOL".into(),
            },
            reserves: [
                (
                    Tok::Stable,
                    ReserveRef {
                        index: 0,
                        decimals: 6,
                    },
                ),
                (
                    Tok::Xlm,
                    ReserveRef {
                        index: 1,
                        decimals: 7,
                    },
                ),
                (
                    Tok::Weth,
                    ReserveRef {
                        index: 2,
                        decimals: 9,
                    },
                ),
            ]
            .into(),
            minted: BTreeMap::new(),
        }
    }

    fn fx_manifest() -> PairManifest {
        PairManifest {
            profile: Profile::StockVsFork,
            description: None,
            net: Net {
                rpc_url: "http://test".into(),
                passphrase: "test".into(),
                config_dir: PathBuf::from("/tmp"),
                headers: Vec::new(),
            },
            identities: BTreeMap::new(),
            a: fx_side("stock"),
            b: fx_side("fork"),
        }
    }

    const RATE_LO: i128 = 1_000_000_000_000;
    const RATE_HI: i128 = 1_000_000_000_010;

    fn clean_snap() -> Snapshot {
        clean_snap_rates(RATE_LO, RATE_LO)
    }

    /// A snapshot that satisfies every recomputed invariant: zero reserves
    /// reconcile against empty strict-map positions, `bstop_rate` 0, PoolEmis
    /// observed absent, a raw ResData row for every configured reserve, every
    /// mock token's balances observed and summing to the empty minted
    /// baseline, all backstop users observed with zero shares/q4w against
    /// zero pool shares, and the LP balance at the backstop matching pool
    /// tokens. Rates are parameters so the fixture can bracket the persisted
    /// rate assertion honestly.
    fn clean_snap_rates(b_rate: i128, d_rate: i128) -> Snapshot {
        let mut positions: BTreeMap<String, Value> = BTreeMap::new();
        for w in ["admin", "frodo", "sam", "user", "pool", "backstop"] {
            positions.insert(
                w.into(),
                json!({"collateral": {}, "liabilities": {}, "supply": {}}),
            );
        }
        // The timeline attributes reserve flow against the reserve's asset
        // address, which a live snapshot always carries; a fixture without it
        // would be "reserve asset not observable", not clean.
        let rd = |asset: &str, index: u32| {
            json!({"asset": asset, "config": {"index": index}, "data": {
                "b_supply": "0", "d_supply": "0",
                "b_rate": b_rate.to_string(), "d_rate": d_rate.to_string(),
                "backstop_credit": "0"
            }})
        };
        let mut balances: BTreeMap<Tok, BTreeMap<String, String>> = Tok::MOCK
            .iter()
            .copied()
            .map(|t| (t, BTreeMap::new()))
            .collect();
        balances.insert(Tok::Lp, [("backstop".to_string(), "0".to_string())].into());
        Snapshot {
            positions,
            reserves: [
                (Tok::Stable, rd("CSTBL", 0)),
                (Tok::Xlm, rd("CXLM", 1)),
                (Tok::Weth, rd("CWETH", 2)),
            ]
            .into(),
            config: json!({"bstop_rate": 0, "oracle": "CORC"}),
            backstop_pool: json!({"shares": "0", "tokens": "0"}),
            backstop_users: Who::ALL
                .iter()
                .copied()
                .map(|w| (w, json!({"shares": "0", "q4w": []})))
                .collect(),
            reward_zone: json!([]),
            balances,
            auctions: BTreeMap::new(),
            raw: [
                ("pool_emis".to_string(), None),
                ("res_data_stable".to_string(), Some(raw_row())),
                ("res_data_xlm".to_string(), Some(raw_row())),
                ("res_data_weth".to_string(), Some(raw_row())),
            ]
            .into(),
        }
    }

    fn raw_row() -> RawEntry {
        RawEntry {
            key: "res".into(),
            val: "0".into(),
            last_modified: 0,
            live_until: None,
        }
    }

    fn fx_step_at(
        scenario: &str,
        index: usize,
        label: &str,
        a: Option<Invocation>,
        b: Option<Invocation>,
    ) -> StepEvidence {
        fx_step_snap(scenario, index, label, a, b, &clean_snap())
    }

    fn fx_step_snap(
        scenario: &str,
        index: usize,
        label: &str,
        a: Option<Invocation>,
        b: Option<Invocation>,
        snap: &Snapshot,
    ) -> StepEvidence {
        let snap = snap.clone();
        let rec = |side: &str, invocation: Option<Invocation>| StepRecord {
            label: label.into(),
            side: side.into(),
            invocation,
            before: Some(snap.clone()),
            snapshot: snap.clone(),
            violations: vec![],
        };
        StepEvidence {
            scenario: scenario.into(),
            index,
            label: label.into(),
            a: rec("stock", a),
            b: rec("fork", b),
            diff: vec![],
            findings: vec![],
        }
    }

    /// The rate assertion is judged on replay from the persisted baseline:
    /// the fixture brackets it honestly with a strictly lower pre-assert
    /// snapshot instead of shipping equal rates. The runner persists no
    /// invocation for this read-only assertion.
    fn fx_rates_step() -> StepEvidence {
        let lo = clean_snap_rates(RATE_LO, RATE_LO);
        let hi = clean_snap_rates(RATE_HI, RATE_HI);
        let rec = |side: &str| StepRecord {
            label: "interest_rates_increased".into(),
            side: side.into(),
            invocation: None,
            before: Some(lo.clone()),
            snapshot: hi.clone(),
            violations: vec![],
        };
        StepEvidence {
            scenario: "interest".into(),
            index: 2,
            label: "interest_rates_increased".into(),
            a: rec("stock"),
            b: rec("fork"),
            diff: vec![],
            findings: vec![],
        }
    }

    fn fx_step(
        index: usize,
        label: &str,
        a: Option<Invocation>,
        b: Option<Invocation>,
    ) -> StepEvidence {
        fx_step_at("interest", index, label, a, b)
    }

    fn trap_code(label: &str) -> u32 {
        crate::scenario::TrapCall::ALL
            .iter()
            .find(|t| t.label() == label)
            .map(|t| t.fork_error())
            .expect("trap label")
    }

    /// The canonical valid scoped single-scenario run — the recipe a corrupting
    /// control starts from: manifest, state.json with the canonical requested
    /// scope + completions, evidence for every non-wait label (wait-only
    /// `interest_wait` is completed but produces no directory, per contract).
    fn write_fx(name: &str) -> (PathBuf, PairManifest) {
        let out = fx_dir(name);
        let m = fx_manifest();
        m.save(&out).unwrap();
        let want = canonical_scope()
            .remove("interest")
            .expect("interest scenario");
        let mut st = RunState::default();
        st.requested.insert("interest".into(), want.clone());
        st.completed.insert("interest".into(), want.clone());
        st.scenario_order = vec!["interest".into()];
        st.save(&out).unwrap();
        write_step(
            &out,
            &fx_step(1, "interest_supply", Some(ok_inv()), Some(ok_inv())),
        )
        .unwrap();
        write_step(&out, &fx_rates_step()).unwrap();
        let hi = clean_snap_rates(RATE_HI, RATE_HI);
        for (n, t) in crate::scenario::traps().into_iter().enumerate() {
            write_step(
                &out,
                &fx_step_snap(
                    "interest",
                    3 + n,
                    t.label,
                    Some(ok_inv()),
                    Some(err_inv(trap_code(t.label))),
                    &hi,
                ),
            )
            .unwrap();
        }
        (out, m)
    }

    /// The smallest canonical scenario whose success creates a liquidation
    /// auction, plus its canonical trap postlude. Clean snapshots satisfy all
    /// other replay checks, isolating the auction observability gate.
    fn write_fx_stale(name: &str) -> (PathBuf, PairManifest) {
        let out = fx_dir(name);
        let m = fx_manifest();
        m.save(&out).unwrap();
        let scope = canonical_scope()
            .remove("stale_auction")
            .expect("stale_auction scenario");
        let mut st = RunState::default();
        st.requested.insert("stale_auction".into(), scope.clone());
        st.completed.insert("stale_auction".into(), scope);
        st.scenario_order = vec!["stale_auction".into()];
        st.save(&out).unwrap();

        let present = || {
            let mut s = clean_snap();
            s.auctions
                .insert(Who::Sam, json!({"block": 1, "bid": {}, "lot": {}}));
            s
        };
        let labels = [
            "stale_reset_prices",
            "stale_mint_sam",
            "stale_sam_positions",
            "stale_reprice_unhealthy",
            "stale_new_auction_full",
            "stale_del_early",
            "stale_del_when_stale",
        ];
        for (n, label) in labels.into_iter().enumerate() {
            let invocation = if label == "stale_del_early" {
                Some(err_inv(1200))
            } else {
                Some(ok_inv())
            };
            let snapshot = if label == "stale_new_auction_full" {
                present()
            } else {
                clean_snap()
            };
            write_step(
                &out,
                &fx_step_snap(
                    "stale_auction",
                    1 + n,
                    label,
                    invocation.clone(),
                    invocation,
                    &snapshot,
                ),
            )
            .unwrap();
        }
        for (n, t) in crate::scenario::traps().into_iter().enumerate() {
            write_step(
                &out,
                &fx_step_snap(
                    "stale_auction",
                    1 + labels.len() + n,
                    t.label,
                    Some(ok_inv()),
                    Some(err_inv(trap_code(t.label))),
                    &clean_snap(),
                ),
            )
            .unwrap();
        }
        (out, m)
    }

    fn tamper_auction_control(steps: &mut [StepEvidence], auction: Option<Value>) {
        let s = steps
            .iter_mut()
            .find(|s| s.label == "stale_new_auction_full")
            .unwrap();
        for r in [&mut s.a, &mut s.b] {
            assert!(r.invocation.as_ref().is_some_and(|i| i.ok));
            assert!(r.violations.is_empty());
            r.snapshot.auctions = auction
                .clone()
                .map(|v| [(Who::Sam, v)].into())
                .unwrap_or_default();
        }
        assert_eq!(
            serde_json::to_value(&s.a.snapshot).unwrap(),
            serde_json::to_value(&s.b.snapshot).unwrap(),
            "the controls must leave both sides' post-snapshots equal"
        );
    }

    fn assert_auction_control_fails(
        out: &Path,
        m: &PairManifest,
        name: &str,
        auction: Option<Value>,
        finding: &str,
    ) {
        let mut steps = load_steps(out, m).unwrap();
        tamper_auction_control(&mut steps, auction);
        let a = analyze(out, m, &steps);
        let s = a
            .steps
            .iter()
            .find(|s| s.label == "stale_new_auction_full")
            .unwrap();
        for side in ['a', 'b'] {
            assert!(
                s.findings
                    .iter()
                    .any(|f| f.starts_with(&format!("{side} ")) && f.contains(finding)),
                "{name} must derive {side}'s auction absence: {:?}",
                s.findings
            );
        }
        assert_eq!(
            serde_json::to_value(&s.a.snapshot).unwrap(),
            serde_json::to_value(&s.b.snapshot).unwrap(),
            "the failure must not be observable from A/B snapshot disagreement"
        );
        assert!(s.a.violations.is_empty() && s.b.violations.is_empty());
        assert!(a.check_failures > 0, "{name}: {:?}", s.findings);
        assert_eq!(a.verdict, "FAIL", "{name}: {:?}", s.findings);
        assert_eq!(verdict_code(&a.verdict), 2, "{name}");
    }

    fn read_md(out: &Path) -> String {
        std::fs::read_to_string(out.join("report.md")).unwrap()
    }

    #[test]
    fn write_step_refuses_to_overwrite_existing_evidence() {
        let out = fx_dir("overwrite");
        let ev = fx_step(1, "x", Some(ok_inv()), Some(ok_inv()));
        write_step(&out, &ev).unwrap();
        let a_path = out.join("evidence/interest/001-x/a.json");
        let first = std::fs::read_to_string(&a_path).unwrap();
        let mut second = ev.clone();
        second.b.invocation.as_mut().unwrap().stderr =
            serde_json::from_value(json!("second attempt")).unwrap();
        assert!(
            write_step(&out, &second).is_err(),
            "retained evidence must not be replaceable"
        );
        assert_eq!(std::fs::read_to_string(&a_path).unwrap(), first);
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn empty_run_reports_incomplete_never_pass() {
        let out = fx_dir("empty");
        let m = fx_manifest();
        m.save(&out).unwrap();
        let steps = load_steps(&out, &m).unwrap();
        let verdict = write_markdown(&out, &m, &steps).unwrap();
        assert_eq!(verdict, "INCOMPLETE");
        assert_eq!(verdict_code(&verdict), 2);
        assert!(read_md(&out).contains("no judged (non-operator) evidence"));
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn legacy_run_without_scope_is_not_certified() {
        let (out, m) = write_fx("legacy");
        let mut st = RunState::load(&out).unwrap();
        st.requested.clear();
        st.save(&out).unwrap();
        let steps = load_steps(&out, &m).unwrap();
        let verdict = write_markdown(&out, &m, &steps).unwrap();
        assert_eq!(verdict, "INCOMPLETE");
        assert!(read_md(&out).contains("no requested scope was recorded"));
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn scoped_single_scenario_fixture_is_certified_pass() {
        let (out, m) = write_fx("scoped-pass");
        let steps = load_steps(&out, &m).unwrap();
        let verdict = write_markdown(&out, &m, &steps).unwrap();
        assert_eq!(verdict, "PASS", "report.md:\n{}", read_md(&out));
        assert_eq!(verdict_code(&verdict), 0);
        let a = analyze(&out, &m, &steps);
        let custody = a.ledger.iter().find(|r| r.clause == "run custody").unwrap();
        assert_eq!(custody.status, "PASS");
        let r1 = a.ledger.iter().find(|r| r.clause.ends_with("r1")).unwrap();
        assert_eq!(
            r1.status, "NOT REQUESTED",
            "freeze clauses were never requested"
        );
        let a11 = a.ledger.iter().find(|r| r.clause == "ADR 0011").unwrap();
        assert_eq!(
            a11.status, "PASS",
            "in-scope traps were judged: {}",
            a11.evidence
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn truncated_requested_scope_is_never_bypassable() {
        let (out, m) = write_fx("trunc");
        let mut st = RunState::load(&out).unwrap();
        let full = st.requested["interest"].clone();
        st.requested.insert("interest".into(), full[..3].to_vec());
        st.save(&out).unwrap();
        let steps = load_steps(&out, &m).unwrap();
        let verdict = write_markdown(&out, &m, &steps).unwrap();
        assert_eq!(verdict, "INCOMPLETE");
        assert!(read_md(&out).contains("truncated or edited"));
        let _ = std::fs::remove_dir_all(&out);
    }

    /// Cross-family contamination can never certify PASS, no matter how
    /// complete each per-scenario label list is.
    #[test]
    fn mixed_family_requested_scope_cannot_certify() {
        let (out, m) = write_fx("mixed-family");
        let mut st = RunState::load(&out).unwrap();
        let extra = canonical_scope()
            .remove("strict_oracle")
            .expect("strict_oracle scope");
        st.requested.insert("strict_oracle".into(), extra.clone());
        st.completed.insert("strict_oracle".into(), extra);
        st.save(&out).unwrap();
        let steps = load_steps(&out, &m).unwrap();
        let verdict = write_markdown(&out, &m, &steps).unwrap();
        assert_ne!(verdict, "PASS", "cross-family scope must never certify");
        assert!(
            read_md(&out).contains("isolated canonical family"),
            "the family rule must be named in the report:\n{}",
            read_md(&out)
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn completed_scope_gap_is_incomplete() {
        let (out, m) = write_fx("gap");
        let mut st = RunState::load(&out).unwrap();
        st.completed
            .get_mut("interest")
            .unwrap()
            .retain(|l| l != "interest_supply");
        st.save(&out).unwrap();
        let steps = load_steps(&out, &m).unwrap();
        let verdict = write_markdown(&out, &m, &steps).unwrap();
        assert_eq!(verdict, "INCOMPLETE");
        assert!(read_md(&out).contains("interest_supply never completed"));
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn pending_records_force_incomplete() {
        let (out, m) = write_fx("pend");
        let d = out.join("pending/interest/001-interest_supply");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("a.intent.json"), "{}").unwrap();
        let steps = load_steps(&out, &m).unwrap();
        let verdict = write_markdown(&out, &m, &steps).unwrap();
        assert_eq!(verdict, "INCOMPLETE");
        assert!(read_md(&out).contains("unresolved pending step"));
        let _ = std::fs::remove_dir_all(&out);
    }

    /// Parent's control: B mirrored to A with poisoned accounting fields and
    /// empty stored violation lists must no longer replay as invariant-clean.
    #[test]
    fn corrupted_equal_snapshots_fail_recomputation() {
        let (out, m) = write_fx("tamper-eq");
        let mut steps = load_steps(&out, &m).unwrap();
        let s = steps
            .iter_mut()
            .find(|s| s.label == "interest_supply")
            .unwrap();
        s.a = s.b.clone();
        for r in [&mut s.a, &mut s.b] {
            let stable = r.snapshot.reserves.get_mut(&Tok::Stable).unwrap();
            stable["data"]["b_rate"] = json!("-1");
            stable["data"]["d_supply"] = json!("999999999999999999");
        }
        let a = analyze(&out, &m, &steps);
        assert_eq!(a.verdict, "FAIL");
        assert!(
            a.invariant_failures > 0,
            "recomputation must derive the failure"
        );
        let acct = a
            .ledger
            .iter()
            .find(|r| r.clause.contains("accounting"))
            .unwrap();
        assert_eq!(acct.status, "FAIL");
        let _ = std::fs::remove_dir_all(&out);
    }

    /// The historical false-pass: a failing ledger row under an overall PASS.
    #[test]
    fn failing_ledger_row_can_no_longer_hide_under_a_pass() {
        let (out, m) = write_fx("ledfail");
        let mut steps = load_steps(&out, &m).unwrap();
        let s = steps
            .iter_mut()
            .find(|s| s.label == "trap_backstop_distribute")
            .unwrap();
        s.b.invocation = Some(ok_inv());
        let a = analyze(&out, &m, &steps);
        let row = a.ledger.iter().find(|r| r.clause == "ADR 0011").unwrap();
        assert_eq!(row.status, "FAIL");
        assert_eq!(a.verdict, "FAIL");
        let _ = std::fs::remove_dir_all(&out);
    }

    /// The retained-failure false-pass: a canonical record whose immutable
    /// stored violation list is non-empty must block certification on replay
    /// and name the failing record in the ledger, even when every current
    /// recomputed check is clean. The stored text carries no authority — a
    /// prose string and an empty string fail alike — and the same string on
    /// operator evidence stays disclosed and unjudged.
    #[test]
    fn retained_canonical_failures_block_certification_with_an_attributable_row() {
        // Clean positive control: the untouched fully scoped fixture
        // certifies through every gate.
        let (out, m) = write_fx("retained-ctrl");
        let steps = load_steps(&out, &m).unwrap();
        assert_eq!(
            write_markdown(&out, &m, &steps).unwrap(),
            "PASS",
            "report.md:\n{}",
            read_md(&out)
        );
        let clean = analyze(&out, &m, &steps);
        assert_eq!(clean.ledger_failures, 0);
        let clean_row = clean
            .ledger
            .iter()
            .find(|r| r.clause == "retained canonical failures")
            .expect("retained-failures row");
        assert_eq!(clean_row.status, "PASS", "{}", clean_row.evidence);
        let _ = std::fs::remove_dir_all(&out);

        for payload in ["retained historical finding", ""] {
            let (out, m) = write_fx("retained-poison");
            let mut steps = load_steps(&out, &m).unwrap();
            let first = steps
                .iter_mut()
                .find(|s| s.label == "interest_supply")
                .unwrap();
            // Both sides of the first canonical record carry the retained
            // string, mirroring what the runner persists on failure.
            first.a.violations.push(
                serde_json::from_value::<crate::stellar::Safe>(serde_json::json!(payload)).unwrap(),
            );
            first.b.violations.push(
                serde_json::from_value::<crate::stellar::Safe>(serde_json::json!(payload)).unwrap(),
            );

            let a = analyze(&out, &m, &steps);
            assert_eq!(a.verdict, "FAIL", "payload {payload:?}");
            let row = a
                .ledger
                .iter()
                .find(|r| r.clause == "retained canonical failures")
                .expect("retained-failures row");
            assert_eq!(row.status, "FAIL", "payload {payload:?}: {}", row.evidence);
            assert!(
                row.evidence.contains("interest_supply"),
                "the failing row must name the retained record: {}",
                row.evidence
            );
            assert!(a.ledger_failures > 0, "payload {payload:?}");
            // The failure reaches the verdict through the shared recompute,
            // not through a prose match: the step carries derived findings.
            let s = a
                .steps
                .iter()
                .find(|s| s.label == "interest_supply")
                .unwrap();
            for side in ['a', 'b'] {
                assert!(
                    s.findings
                        .iter()
                        .any(|f| f.starts_with(&format!("{side} ")) && f.contains("retained")),
                    "{side} must carry a retained-failure finding: {:?}",
                    s.findings
                );
            }
            // Report path and a second replay agree, without accumulating.
            assert_eq!(write_markdown(&out, &m, &steps).unwrap(), "FAIL");
            let again = analyze(&out, &m, &steps);
            assert_eq!(again.verdict, "FAIL");
            assert_eq!(again.check_failures, a.check_failures);
            assert_eq!(again.ledger_failures, a.ledger_failures);
            // The stored strings survive untouched.
            let s = again
                .steps
                .iter()
                .find(|s| s.label == "interest_supply")
                .unwrap();
            assert_eq!(s.a.violations, s.b.violations);
            assert_eq!(
                s.a.violations.last().map(|v| v.to_string()),
                Some(payload.to_string())
            );
            let _ = std::fs::remove_dir_all(&out);
        }

        // Operator isolation: the same retained string on operator evidence
        // is disclosed, never judged, and cannot turn a clean run red.
        let (out, m) = write_fx("retained-operator");
        let mut op = fx_step_at(
            crate::run::OPERATOR_SCENARIO,
            99,
            "manual_audit",
            Some(ok_inv()),
            Some(ok_inv()),
        );
        op.a.violations.push(
            serde_json::from_value::<crate::stellar::Safe>(serde_json::json!(
                "retained historical finding"
            ))
            .unwrap(),
        );
        op.b.violations.push(
            serde_json::from_value::<crate::stellar::Safe>(serde_json::json!(
                "retained historical finding"
            ))
            .unwrap(),
        );
        crate::report::write_step(&out, &op).unwrap();
        let steps = load_steps(&out, &m).unwrap();
        let a = analyze(&out, &m, &steps);
        assert_eq!(
            a.verdict,
            "PASS",
            "operator evidence tainted the verdict: {:?}",
            a.ledger
                .iter()
                .filter(|r| r.status == "FAIL")
                .map(|r| &r.clause)
                .collect::<Vec<_>>()
        );
        assert_eq!(a.ledger_failures, 0);
        assert!(
            a.steps
                .iter()
                .any(|s| s.scenario == crate::run::OPERATOR_SCENARIO && !s.a.violations.is_empty()),
            "the operator's retained string must still be disclosed"
        );
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn every_recorded_trap_occurrence_is_judged() {
        let first = fx_step_at(
            "lending",
            12,
            "trap_backstop_distribute",
            Some(ok_inv()),
            Some(err_inv(1000)),
        );
        let later = fx_step_at(
            "liquidation",
            12,
            "trap_backstop_distribute",
            Some(ok_inv()),
            Some(ok_inv()),
        );
        let scope: BTreeSet<String> = ["trap_backstop_distribute"]
            .map(|s| s.to_string())
            .into_iter()
            .collect();
        let rows = ledger_rows(&fx_manifest(), &[first, later], Some(&scope));
        let row = rows.iter().find(|r| r.clause == "ADR 0011").unwrap();
        assert_eq!(
            row.status, "FAIL",
            "the first (passing) occurrence must not certify the later success: {}",
            row.evidence
        );
    }

    #[test]
    fn missing_observations_are_never_vacuous_passes() {
        let (out, m) = write_fx("missobs");
        let mut steps = load_steps(&out, &m).unwrap();
        for s in steps.iter_mut() {
            s.b.snapshot.reward_zone = Value::Null;
            s.b.snapshot.positions.insert("pool".into(), Value::Null);
            s.b.snapshot.raw.remove("pool_emis");
        }
        let a = analyze(&out, &m, &steps);
        let drained = a
            .ledger
            .iter()
            .find(|r| r.clause.contains("drained"))
            .unwrap();
        assert_eq!(
            drained.status, "FAIL",
            "null reward zone / null pool is not emptiness proof"
        );
        let emis = a
            .ledger
            .iter()
            .find(|r| r.clause.contains("r7 / ADR 0011"))
            .unwrap();
        assert_eq!(
            emis.status, "FAIL",
            "a missing raw row is not observed absence"
        );
        assert_eq!(a.verdict, "FAIL");
        let _ = std::fs::remove_dir_all(&out);
    }

    #[test]
    fn missing_baselines_refuse_certification() {
        let (out, m) = write_fx("nobefore");
        let mut steps = load_steps(&out, &m).unwrap();
        for s in steps.iter_mut() {
            s.a.before = None;
            s.b.before = None;
        }
        let a = analyze(&out, &m, &steps);
        assert!(
            a.check_failures > 0,
            "state-change claims cannot be certified without a baseline"
        );
        assert_eq!(a.verdict, "FAIL");
        let _ = std::fs::remove_dir_all(&out);
    }

    /// Parent's false-pass control: a rejected call that left raw storage
    /// altered must fail the immutability check, the clause certifying it,
    /// and the verdict — not hide behind empty stored violation lists.
    #[test]
    fn altered_raw_on_rejected_call_fails_its_clause_and_verdict() {
        let (out, m) = write_fx("tamper-rejected");
        let mut steps = load_steps(&out, &m).unwrap();
        let s = steps
            .iter_mut()
            .find(|s| s.label == "trap_backstop_claim")
            .unwrap();
        s.b.snapshot.raw.insert(
            "beemis".into(),
            Some(RawEntry {
                key: "beemis".into(),
                val: "0".into(),
                last_modified: 0,
                live_until: None,
            }),
        );
        let a = analyze(&out, &m, &steps);
        assert!(
            a.steps.iter().any(|s| s
                .findings
                .iter()
                .any(|f| f.contains("rejected-call storage"))),
            "the altered raw row must be derived on replay"
        );
        let row = a.ledger.iter().find(|r| r.clause == "ADR 0011").unwrap();
        assert_eq!(row.status, "FAIL", "{}", row.evidence);
        assert_eq!(a.verdict, "FAIL");
        let _ = std::fs::remove_dir_all(&out);
    }

    /// The rate assertion is a scenario claim, not stored prose: equal
    /// baseline/after rates on the assert step fail replay even though every
    /// stored violation list is empty.
    #[test]
    fn equal_rates_fail_the_rate_assertion_on_replay() {
        let (out, m) = write_fx("equalrates");
        let mut steps = load_steps(&out, &m).unwrap();
        let s = steps
            .iter_mut()
            .find(|s| s.label == "interest_rates_increased")
            .unwrap();
        for r in [&mut s.a, &mut s.b] {
            r.before = Some(r.snapshot.clone());
        }
        let a = analyze(&out, &m, &steps);
        assert!(
            a.check_failures > 0,
            "the equal-rate mutation must be derived on replay"
        );
        assert_eq!(a.verdict, "FAIL");
        let _ = std::fs::remove_dir_all(&out);
    }

    /// Auction-witness positive control at the recompute/derived-finding
    /// boundary: a successful `new_auction` whose post-snapshot observes the
    /// auction raises no finding on either side. The run itself must NOT
    /// certify PASS: this fixture's recorded scope is `stale_auction` alone,
    /// which lacks its `liquidation` prerequisite, and the report must say
    /// so — an out-of-isolation scope is never certifiable even when every
    /// judged predicate holds. The canonical full-PASS control is
    /// `scoped_single_scenario_fixture_is_certified_pass`.
    #[test]
    fn successful_new_auction_raises_no_witness_finding_in_a_non_certifiable_scope() {
        let (out, m) = write_fx_stale("auction-present");
        let steps = load_steps(&out, &m).unwrap();
        let s = steps
            .iter()
            .find(|s| s.label == "stale_new_auction_full")
            .unwrap();
        assert!(
            s.findings.is_empty(),
            "an observed auction must raise no witness finding: {:?}",
            s.findings
        );
        let verdict = write_markdown(&out, &m, &steps).unwrap();
        assert_eq!(
            verdict,
            "INCOMPLETE",
            "a prerequisite-less synthetic scope must not certify PASS:\n{}",
            read_md(&out)
        );
        assert!(read_md(&out).contains("isolated canonical family"));
        let _ = std::fs::remove_dir_all(&out);
    }

    /// A successful new_auction cannot hide a silently-absent auction: equal
    /// snapshots and empty stored violation lists must still fail on replay.
    #[test]
    fn successful_new_auction_cannot_certify_missing_or_malformed_auction() {
        let (out, m) = write_fx_stale("auction-absent");
        for (name, auction, finding) in [
            ("entry vanished", None, "not observed"),
            ("observed absent", Some(Value::Null), "observed absent"),
            (
                "malformed",
                Some(json!({"block": 1, "bid": {}, "lot": null})),
                "malformed",
            ),
        ] {
            assert_auction_control_fails(&out, &m, name, auction, finding);
        }
        let _ = std::fs::remove_dir_all(&out);
    }

    /// A rate fall with no default is a timeline failure even when the run's
    /// custody is complete: the scope cannot certify what §8 says is broken.
    #[test]
    fn unexplained_rate_decrease_fails_a_complete_scope() {
        let (out, m) = write_fx("rate-fall");
        let steps = load_steps(&out, &m).unwrap();
        let a = analyze(&out, &m, &steps);
        assert_eq!(a.verdict, "PASS", "baseline: {}", read_md(&out));
        assert_eq!(a.timeline_violations, 0, "baseline must be clean");
        let mut steps = steps;
        let s = steps
            .iter_mut()
            .find(|s| s.label == "trap_backstop_claim")
            .unwrap();
        s.b.snapshot.reserves.get_mut(&Tok::Stable).unwrap()["data"]["b_rate"] =
            json!(RATE_LO - 1_000_000);
        let a = analyze(&out, &m, &steps);
        assert_eq!(a.timeline_violations, 1, "{:?}", a.ledger);
        assert_eq!(a.verdict, "FAIL");
        let custody = a
            .ledger
            .iter()
            .find(|r| r.clause == "run custody")
            .expect("custody row");
        assert_eq!(custody.status, "PASS");
        let _ = std::fs::remove_dir_all(&out);
    }

    /// A real `liq_fill_final` record carries `fill_auction` beside the loss
    /// event. The fill transfers positions; it must not excuse a debt that
    /// stops reconciling. Identical snapshots, only the event amount moves,
    /// judged all the way through the analyze verdict.
    #[test]
    fn a_liquidation_fill_must_not_waive_the_loss_residual_through_the_verdict() {
        for (amount, want) in [("100", "PASS"), ("101", "FAIL")] {
            let (out, m) = write_fx(&format!("fill-{amount}"));
            let mut steps = load_steps(&out, &m).unwrap();
            let s = steps
                .iter_mut()
                .find(|s| s.label == "interest_supply")
                .unwrap();
            let mut before = clean_snap();
            before.reserves.get_mut(&Tok::Stable).unwrap()["data"]["d_supply"] = json!("100");
            before.positions.get_mut("user").unwrap()["liabilities"] = json!({"0": "100"});
            s.b.before = Some(before);
            s.b.invocation = Some(
                serde_json::from_value(json!({
                    "ok": true,
                    "value": null,
                    "error": null,
                    "events": [{
                        "contract": "CPOOL",
                        "topics": [{"symbol": "fill_auction"}, {"address": "CSTBL"}],
                        "value": null
                    }, {
                        "contract": "CPOOL",
                        "topics": [{"symbol": "defaulted_debt"}, {"address": "GUSER"}, {"address": "CSTBL"}],
                        "value": {"i128": amount}
                    }],
                    "stderr": ""
                }))
                .unwrap(),
            );
            let a = analyze(&out, &m, &steps);
            let judged = a
                .steps
                .iter()
                .find(|x| x.label == "interest_supply")
                .unwrap();
            assert_eq!(a.verdict, want, "amount {amount}: {:?}", judged.findings);
            assert_eq!(a.invariant_failures, 0, "amount {amount}");
            if want == "PASS" {
                assert_eq!(a.check_failures, 0, "amount {amount}");
                assert_eq!(a.timeline_violations, 0, "amount {amount}");
            } else {
                // A flow residual is a recomputed finding: it must surface on
                // the step, not merely inflate a prose counter.
                assert_eq!(a.check_failures, 1, "amount {amount}");
                assert_eq!(a.timeline_violations, 1, "amount {amount}");
                assert!(
                    judged
                        .findings
                        .iter()
                        .any(|f| f.contains("timeline") && f.contains("residual 1")),
                    "{:?}",
                    judged.findings
                );
            }
            let _ = std::fs::remove_dir_all(&out);
        }
    }

    /// A diagnostic snapshot that predates the campaign's latest rate is
    /// disclosed but must not contaminate the verdict-bearing timeline.
    #[test]
    fn operator_old_snapshot_cannot_turn_a_clean_run_red() {
        let (out, m) = write_fx("operator-old");
        let op = fx_step_snap(
            crate::run::OPERATOR_SCENARIO,
            1,
            "snapshot_only",
            None,
            None,
            &clean_snap_rates(RATE_LO, RATE_LO),
        );
        crate::report::write_step(&out, &op).unwrap();
        let steps = load_steps(&out, &m).unwrap();
        let a = analyze(&out, &m, &steps);
        assert_eq!(
            a.steps
                .iter()
                .filter(|s| s.scenario == crate::run::OPERATOR_SCENARIO)
                .count(),
            1,
            "operator evidence retained"
        );
        assert_eq!(a.timeline_violations, 0, "operator evidence judged");
        assert_eq!(a.verdict, "PASS", "report.md:\n{}", read_md(&out));
        let verdict = write_markdown(&out, &m, &steps).unwrap();
        assert_eq!(verdict, "PASS");
        assert!(read_md(&out).contains("operator action(s) were performed"));
        let _ = std::fs::remove_dir_all(&out);
    }

    /// An invocation record claiming success while carrying a contract error
    /// is not evidence: the expectation checker must refuse it on replay.
    #[test]
    fn contradictory_successful_invocation_cannot_pass_recompute() {
        let (out, m) = write_fx("contradict");
        let mut steps = load_steps(&out, &m).unwrap();
        let s = steps
            .iter_mut()
            .find(|s| s.label == "trap_backstop_distribute")
            .unwrap();
        s.b.invocation = Some(
            serde_json::from_value(json!({
                "ok": true,
                "value": null,
                "error": {"Contract": 1000},
                "events": [],
                "stderr": ""
            }))
            .unwrap(),
        );
        let a = analyze(&out, &m, &steps);
        assert!(
            a.steps.iter().any(|s| s
                .findings
                .iter()
                .any(|f| f.contains("contradictory invocation"))),
            "the contradiction must be derived on replay"
        );
        assert_eq!(a.verdict, "FAIL");
        let _ = std::fs::remove_dir_all(&out);
    }

    /// A real SEP-41 mint event: topics name the admin and the recipient, and
    /// only the emitting contract identifies the token. Replay must credit the
    /// side's own stable token so conservation passes.
    #[test]
    fn sep41_mint_event_is_credited_by_emitting_contract_identity() {
        let m = fx_manifest();
        let mut steps = mint_replay(
            &mint_snap("5000000000"),
            vec![mint_ev("CSTBL", "GAIRCADMIN", "5000000000")],
        );
        recompute(&m, &mut steps);
        assert!(steps[0].findings.is_empty(), "{:?}", steps[0].findings);
    }

    /// A foreign emitter cannot mint the side's token by naming its address
    /// in the topics: only the emitting contract credits supply, so the
    /// unbacked balance stays a conservation finding.
    #[test]
    fn spoofed_mint_topic_cannot_credit_supply_from_a_foreign_emitter() {
        let m = fx_manifest();
        let mut steps = mint_replay(
            &mint_snap("5000000000"),
            vec![mint_ev("CFOREIGN", "CSTBL", "5000000000")],
        );
        recompute(&m, &mut steps);
        assert_eq!(
            steps[0]
                .findings
                .iter()
                .filter(|f| f.contains("stable: balances 5000000000 != minted 0"))
                .count(),
            2,
            "both sides must derive the unbacked supply: {:?}",
            steps[0].findings
        );
    }

    /// Garbled shape (truncated arity, multi-tag topic or value), negative,
    /// unparseable, out-of-i64-range, and cumulatively-overflowing mint
    /// events fail closed as accounting findings: no panic, no wrap, and no
    /// supply credit — including from a contradictory invocation.
    #[test]
    fn invalid_mint_amount_cannot_panic_or_certify() {
        let m = fx_manifest();
        for amount in ["-5", "not-a-number", "9223372036854775808"] {
            let mut steps = mint_replay(
                &mint_snap("0"),
                vec![mint_ev("CSTBL", "GAIRCADMIN", amount)],
            );
            recompute(&m, &mut steps);
            assert_eq!(
                steps[0].findings.len(),
                2,
                "{amount}: {:?}",
                steps[0].findings
            );
            assert!(
                steps[0]
                    .findings
                    .iter()
                    .all(|f| f.contains("invariant:") && f.contains("mint amount is invalid")),
                "{amount}: {:?}",
                steps[0].findings
            );
        }
        // A garbled mint event from an authenticated token contract is
        // malformed data: it must fail loudly and credit nothing.
        let malformed = [
            // truncated arity
            crate::stellar::Event {
                contract: "CSTBL".into(),
                topics: vec![json!({"symbol": "mint"}), json!({"address": "GAIRCADMIN"})],
                value: json!({"i128": "5000000000"}),
            },
            // multi-tag topic
            crate::stellar::Event {
                contract: "CSTBL".into(),
                topics: vec![
                    json!({"symbol": "mint", "address": "GAIRCADMIN"}),
                    json!({"address": "GAIRCADMIN"}),
                    json!({"address": "GBGHRECIPIENT"}),
                ],
                value: json!({"i128": "5000000000"}),
            },
            // multi-tag value
            crate::stellar::Event {
                contract: "CSTBL".into(),
                topics: vec![
                    json!({"symbol": "mint"}),
                    json!({"address": "GAIRCADMIN"}),
                    json!({"address": "GBGHRECIPIENT"}),
                ],
                value: json!({"i128": "5000000000", "u32": 1}),
            },
        ];
        for ev in malformed {
            let mut steps = mint_replay(&mint_snap("5000000000"), vec![ev]);
            recompute(&m, &mut steps);
            assert_eq!(
                steps[0]
                    .findings
                    .iter()
                    .filter(|f| f.contains("mint event is malformed"))
                    .count(),
                2,
                "garbled shapes must fail loudly: {:?}",
                steps[0].findings
            );
            assert_eq!(
                steps[0]
                    .findings
                    .iter()
                    .filter(|f| f.contains("stable: balances 5000000000 != minted 0"))
                    .count(),
                2,
                "a garbled mint shape cannot credit supply: {:?}",
                steps[0].findings
            );
        }
        let big = "9223372036854775807";
        let mut steps = mint_replay(
            &mint_snap(big),
            vec![
                mint_ev("CSTBL", "GAIRCADMIN", big),
                mint_ev("CSTBL", "GAIRCADMIN", big),
            ],
        );
        recompute(&m, &mut steps);
        assert_eq!(steps[0].findings.len(), 2, "{:?}", steps[0].findings);
        assert!(
            steps[0]
                .findings
                .iter()
                .all(|f| f.contains("cumulative supply overflows")),
            "{:?}",
            steps[0].findings
        );
        let mut steps = mint_replay_err(
            &mint_snap("5000000000"),
            vec![mint_ev("CSTBL", "GAIRCADMIN", "5000000000")],
            Some(1000),
        );
        recompute(&m, &mut steps);
        assert_eq!(
            steps[0]
                .findings
                .iter()
                .filter(|f| f.contains("contradictory invocation"))
                .count(),
            2,
            "{:?}",
            steps[0].findings
        );
        assert_eq!(
            steps[0]
                .findings
                .iter()
                .filter(|f| f.contains("stable: balances 5000000000 != minted 0"))
                .count(),
            2,
            "a contradictory invocation cannot credit supply: {:?}",
            steps[0].findings
        );
    }

    fn mint_ev(contract: &str, topic1: &str, amount: &str) -> crate::stellar::Event {
        crate::stellar::Event {
            contract: contract.into(),
            topics: vec![
                json!({"symbol": "mint"}),
                json!({"address": topic1}),
                json!({"address": "GBGHRECIPIENT"}),
            ],
            value: json!({"i128": amount}),
        }
    }

    fn mint_snap(balance: &str) -> Snapshot {
        let mut snap = clean_snap();
        snap.balances
            .get_mut(&Tok::Stable)
            .unwrap()
            .insert("frodo".into(), balance.into());
        snap
    }

    /// One `oracle_mint_frodo` step whose successful invocation carries these
    /// mint events on both sides, replayed against the fixture manifest.
    fn mint_replay(snap: &Snapshot, events: Vec<crate::stellar::Event>) -> Vec<StepEvidence> {
        mint_replay_err(snap, events, None)
    }

    fn mint_replay_err(
        snap: &Snapshot,
        events: Vec<crate::stellar::Event>,
        error: Option<u32>,
    ) -> Vec<StepEvidence> {
        let rec = |side: &str| StepRecord {
            label: "oracle_mint_frodo".into(),
            side: side.into(),
            invocation: Some(
                serde_json::from_value(json!({
                    "ok": true,
                    "value": null,
                    "error": error.map(|c| json!({"Contract": c})),
                    "events": serde_json::to_value(&events).unwrap(),
                    "stderr": ""
                }))
                .unwrap(),
            ),
            before: Some(clean_snap()),
            snapshot: snap.clone(),
            violations: vec![],
        };
        vec![StepEvidence {
            scenario: "strict_oracle".into(),
            index: 0,
            label: "oracle_mint_frodo".into(),
            a: rec("stock"),
            b: rec("fork"),
            diff: vec![],
            findings: vec![],
        }]
    }

    /// r7 admission uses the shared timeline grammar: foreign-contract and
    /// unconfigured-asset loss events never certify the clause.
    #[test]
    fn r7_requires_a_genuinely_sourced_positive_loss_event() {
        let m = fx_manifest();
        let scope: BTreeSet<String> = ["liq_fill_final"]
            .map(|s| s.to_string())
            .into_iter()
            .collect();
        let loss_step = |contract: &str, asset: &str, amount: &str| StepEvidence {
            scenario: "liquidation".into(),
            index: 1,
            label: "liq_fill_final".into(),
            a: rec("stock", Some(ok_inv()), vec![]),
            b: {
                let mut r = rec("fork", Some(ok_inv()), vec![]);
                r.invocation.as_mut().unwrap().events = vec![crate::stellar::Event {
                    contract: contract.into(),
                    topics: vec![
                        json!({"symbol": "defaulted_debt"}),
                        json!({"address": "GUSER"}),
                        json!({"address": asset}),
                    ],
                    value: json!({"i128": amount}),
                }];
                r
            },
            diff: vec![],
            findings: vec![],
        };
        let rows = ledger_rows(&m, &[loss_step("CPOOL", "CSTBL", "100")], Some(&scope));
        let row = rows.iter().find(|r| r.clause.ends_with("r7")).unwrap();
        assert_eq!(row.status, "PASS", "{}", row.evidence);
        assert!(row.evidence.contains("across all 1 judged step(s)"));
        for (contract, asset, name) in [
            ("CGONER", "CSTBL", "foreign contract"),
            ("CPOOL", "CUNKNOWN", "unconfigured asset"),
        ] {
            let rows = ledger_rows(&m, &[loss_step(contract, asset, "100")], Some(&scope));
            let row = rows.iter().find(|r| r.clause.ends_with("r7")).unwrap();
            assert_eq!(
                row.status, "FAIL",
                "{name} must not arm r7: {}",
                row.evidence
            );
        }
        // A good early occurrence must not hide a later malformed claim: the
        // claim is selected first, and only genuine evidence authenticates it.
        for (contract, asset, amount, name) in [
            ("CGONER", "CSTBL", "100", "forged later contract"),
            ("CPOOL", "CSTBL", "0", "zero-amount later claim"),
        ] {
            let rows = ledger_rows(
                &m,
                &[
                    loss_step("CPOOL", "CSTBL", "100"),
                    loss_step(contract, asset, amount),
                ],
                Some(&scope),
            );
            let row = rows.iter().find(|r| r.clause.ends_with("r7")).unwrap();
            assert_eq!(
                row.status, "FAIL",
                "{name} must fail r7 even behind a valid occurrence: {}",
                row.evidence
            );
        }
    }

    /// Operator records are disclosed, never judged: the same reward-zone,
    /// backstop-credit and PoolEmis violations that fail a canonical step
    /// must leave a complete canonical scope certified.
    #[test]
    fn operator_poison_cannot_taint_the_canonical_verdict() {
        let poison = || {
            let mut snap = clean_snap_rates(RATE_LO, RATE_LO);
            snap.reward_zone = json!([{"credit": "1"}]);
            snap.reserves.get_mut(&Tok::Stable).unwrap()["data"]["backstop_credit"] = json!("1");
            snap.raw.insert("pool_emis".into(), Some(raw_row()));
            snap
        };

        // Negative isolation: the poison is real — on canonical evidence it fails.
        let (out, m) = write_fx("operator-poison-canonical");
        let mut steps = load_steps(&out, &m).unwrap();
        steps
            .iter_mut()
            .find(|s| s.label == "interest_supply")
            .unwrap()
            .b
            .snapshot = poison();
        let a = analyze(&out, &m, &steps);
        assert_eq!(
            a.verdict, "FAIL",
            "the poison must judge canonical evidence"
        );
        assert!(a.ledger_failures > 0, "{:?}", a.ledger);
        let _ = std::fs::remove_dir_all(&out);

        // Same poison as operator evidence: disclosed, excluded, certified run.
        let (out, m) = write_fx("operator-poison-operator");
        let op = fx_step_snap(
            crate::run::OPERATOR_SCENARIO,
            99,
            "manual_audit",
            None,
            None,
            &poison(),
        );
        crate::report::write_step(&out, &op).unwrap();
        let steps = load_steps(&out, &m).unwrap();
        let a = analyze(&out, &m, &steps);
        assert_eq!(
            a.ledger_failures,
            0,
            "operator evidence contaminated the ledger: {:?}",
            a.ledger
                .iter()
                .filter(|r| r.status == "FAIL")
                .map(|r| (&r.clause, &r.evidence))
                .collect::<Vec<_>>()
        );
        assert_eq!(a.timeline_violations, 0);
        assert_eq!(a.verdict, "PASS", "report.md:\n{}", read_md(&out));
        assert_eq!(
            a.steps
                .iter()
                .filter(|s| s.scenario == crate::run::OPERATOR_SCENARIO)
                .count(),
            1,
            "operator evidence must still be retained"
        );
        let _ = std::fs::remove_dir_all(&out);
    }
}
